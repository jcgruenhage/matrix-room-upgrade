use std::collections::HashSet;
use std::fs::File;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use js_int::{int, Int};
use log::{debug, error, info, warn, LevelFilter};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

mod cli;
mod config;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    // LevelFilter::iter() runs from Off to Trace, so index 3 is the default of Info.
    let log_level = LevelFilter::iter()
        .nth((3 + usize::from(cli.verbose)).saturating_sub(usize::from(cli.quiet)))
        .unwrap_or(LevelFilter::Trace);
    env_logger::Builder::new()
        .filter_level(log_level)
        .parse_default_env()
        .init();

    let config_file = File::open(&cli.config)
        .with_context(|| format!("failed to open {}", cli.config.display()))?;
    let config: config::Config = serde_yaml::from_reader(config_file)
        .with_context(|| format!("failed to parse {}", cli.config.display()))?;
    let dirs = ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
        .context("failed to determine the home directory")?;
    let mut state = state::State::load(
        dirs.state_dir()
            .unwrap_or(dirs.data_local_dir())
            .join("state.json"),
    )?;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {}", config.access_token))
            .context("access token is not a valid header value")?,
    );

    let http_client = reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .default_headers(headers)
        .build()?;

    let self_user_id_res = send(http_client.get(format!(
        "{}/_matrix/client/v3/account/whoami",
        config.homeserver_url
    )))
    .await?
    .json::<Value>()
    .await?;
    let self_user_id = self_user_id_res["user_id"]
        .as_str()
        .context("whoami response has no user_id")?
        .to_string();
    debug!("Logged in as {self_user_id}");

    // Prepare all rooms first, so that everything that needs asking is asked up front.
    let mut failed_rooms = Vec::new();
    let mut prepared_rooms = Vec::new();
    for room in &config.rooms {
        match prepare_room(&http_client, &config.homeserver_url, &self_user_id, room).await {
            Ok(steps) => prepared_rooms.push((room, steps)),
            Err(err) => {
                error!("Failed to prepare {room}: {err:#}");
                failed_rooms.push(room);
            }
        }
    }
    for (room, steps) in prepared_rooms {
        if let Err(err) = upgrade_room(
            &http_client,
            &config,
            &mut state,
            &self_user_id,
            room,
            steps,
        )
        .await
        {
            error!("Failed to upgrade {room}: {err:#}");
            failed_rooms.push(room);
        }
    }
    anyhow::ensure!(
        failed_rooms.is_empty(),
        "failed to upgrade {failed_rooms:?}"
    );
    Ok(())
}

/// Posts an upgrade notice in `room` and creates the room replacing it, returning its ID.
async fn create_replacement_room(
    http_client: &reqwest::Client,
    config: &config::Config,
    self_user_id: &str,
    room: &str,
) -> anyhow::Result<String> {
    let mut power_levels = get_state(
        http_client,
        &config.homeserver_url,
        room,
        "m.room.power_levels",
        "",
    )
    .await?
    .context("room has no power levels")?;
    let map = power_levels
        .as_object_mut()
        .context("PL state is not an object")?;
    normalize_power_levels(map)?;
    let users_default = power_level(map, "users_default", int!(0))?;
    let users = map
        .entry("users")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("PL state key users is not an object")?;

    // The creators of the old room lose their unlimited power in the new one, so they get as much
    // power as anyone else has instead, but at least 100, unless they are dropped.
    let room_state = send(http_client.get(format!(
        "{}/_matrix/client/v3/rooms/{room}/state",
        config.homeserver_url
    )))
    .await?
    .json::<Value>()
    .await?;
    let create = room_state
        .as_array()
        .context("room state is not an array")?
        .iter()
        .find(|event| event["type"] == "m.room.create" && event["state_key"] == "")
        .context("room has no create event")?;
    let creator_level = users
        .iter()
        .map(|(user, level)| parse_power_level(user, level))
        .try_fold(int!(100), |max, level| anyhow::Ok(max.max(level?)))?;
    for creator in creators(create)
        .into_iter()
        .filter(|creator| !config.drop_members.iter().any(|member| member == creator))
    {
        users.insert(creator.to_string(), json!(creator_level));
    }

    for (user_id, pl) in config.pl_overrides.iter() {
        if users_default == *pl {
            users.remove(user_id);
        } else {
            users.insert(user_id.to_string(), json!(*pl));
        }
        info!("Overrode power level for {user_id} to be {pl}")
    }

    if config.target_room_version >= 12 {
        users.remove(self_user_id);
    }

    let mut initial_state = Vec::new();
    for event_type in &config.state_events_to_transfer {
        // Power levels are passed separately as power_level_content_override, and the canonical
        // alias can only be set once its aliases point to the new room, see move_aliases.
        if event_type == "m.room.power_levels" || event_type == "m.room.canonical_alias" {
            continue;
        }
        if let Some(content) =
            get_state(http_client, &config.homeserver_url, room, event_type, "").await?
        {
            initial_state.push(json!({
                "content": content,
                "type": event_type,
            }));
        }
    }
    // Without a preset, createRoom uses private_chat, which lets guests join. Rooms without guest
    // access don't, so we keep that unless the old room's guest access is transferred.
    if !initial_state
        .iter()
        .any(|event| event["type"] == "m.room.guest_access")
    {
        initial_state.push(json!({
            "content": { "guest_access": "forbidden" },
            "type": "m.room.guest_access",
        }));
    }
    debug!("New state: {initial_state:#?}, power levels: {power_levels:#?}");

    let txn_id = Uuid::new_v4();
    let res = send(
        http_client
            .put(format!(
                "{}/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn_id}",
                config.homeserver_url
            ))
            .json(&json!({
                "body": "Upgrading room, please stand by",
                "msgtype": "m.text"
            }
            )),
    )
    .await?;
    let last_event_id = res.json::<Value>().await?["event_id"]
        .as_str()
        .context("event_id is not a string")?
        .to_string();

    debug!("Last event ID: {last_event_id}");

    let target_room_version = format!("{}", config.target_room_version);

    let new_room_body = send(
        http_client
            .post(format!(
                "{}/_matrix/client/v3/createRoom",
                config.homeserver_url
            ))
            .json(&json!({
                "creation_content": {
                    "predecessor": {
                        "event_id": last_event_id,
                        "room_id": room,
                    },
                },
                "room_version": target_room_version,
                "power_level_content_override": power_levels,
                "initial_state": initial_state,
            })),
    )
    .await?
    .json::<Value>()
    .await?;
    Ok(new_room_body["room_id"]
        .as_str()
        .context("room id is not a string")?
        .to_string())
}

/// The optional steps of upgrading a room that we have enough power for, or were told to do
/// without.
struct Steps {
    lock_down: bool,
    move_aliases: bool,
}

/// Joins `room` and checks that we have enough power to upgrade it.
///
/// If we don't, this offers to take over the power of the local user with the most power
/// through Synapse's make_room_admin admin API, and to skip the steps we still can't do.
async fn prepare_room(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    self_user_id: &str,
    room: &str,
) -> anyhow::Result<Steps> {
    // The admin API also works for rooms we aren't in yet, but only server admins can use it and
    // reverse proxies often don't expose it.
    let res = send_retrying(http_client.get(format!(
        "{homeserver_url}/_synapse/admin/v1/rooms/{room}/state"
    )))
    .await?;
    let admin_api = !matches!(res.status(), StatusCode::FORBIDDEN | StatusCode::NOT_FOUND);
    let room_state = if admin_api {
        error_for_status(res).await?.json::<Value>().await?["state"].take()
    } else {
        join(http_client, homeserver_url, room).await?;
        send(http_client.get(format!(
            "{homeserver_url}/_matrix/client/v3/rooms/{room}/state"
        )))
        .await?
        .json::<Value>()
        .await?
    };
    let room_state = room_state
        .as_array()
        .context("room state is not an array")?;
    let find = |event_type: &str, state_key: &str| {
        room_state
            .iter()
            .find(|event| event["type"] == event_type && event["state_key"] == state_key)
    };

    let creators = creators(find("m.room.create", "").context("room has no create event")?);
    let power_levels = match find("m.room.power_levels", "") {
        Some(power_levels) if !creators.contains(&self_user_id) => power_levels,
        _ => {
            join(http_client, homeserver_url, room).await?;
            return Ok(Steps {
                lock_down: true,
                move_aliases: true,
            });
        }
    };
    let power_levels = power_levels["content"]
        .as_object()
        .context("PL state is not an object")?;
    let empty = serde_json::Map::new();
    let map = |key: &str| match power_levels.get(key) {
        Some(map) => map
            .as_object()
            .with_context(|| format!("PL state key {key} is not an object")),
        None => Ok(&empty),
    };
    let (events, users) = (map("events")?, map("users")?);
    let state_default = power_level(power_levels, "state_default", int!(50))?;
    let events_default = power_level(power_levels, "events_default", int!(0))?;
    let users_default = power_level(power_levels, "users_default", int!(0))?;

    // The power needed for each step, counting only changes that are still to be made.
    let upgrade = if find("m.room.tombstone", "").is_some() {
        int!(0)
    } else {
        power_level(events, "m.room.tombstone", state_default)?.max(power_level(
            events,
            "m.room.message",
            events_default,
        )?)
    };
    let mut lock_down = int!(0);
    let restricted = int!(50).max(users_default.saturating_add(int!(1)));
    if events_default < restricted || power_level(power_levels, "invite", int!(0))? < restricted {
        lock_down = restricted.max(power_level(events, "m.room.power_levels", state_default)?);
    }
    if find("m.room.join_rules", "").is_some_and(|event| event["content"]["join_rule"] != "invite")
    {
        lock_down = lock_down.max(power_level(events, "m.room.join_rules", state_default)?);
    }
    let move_aliases = if find("m.room.canonical_alias", "").is_some_and(|event| {
        event["content"]
            .as_object()
            .is_some_and(|content| !content.is_empty())
    }) {
        power_level(events, "m.room.canonical_alias", state_default)?
    } else {
        int!(0)
    };

    let mut level = power_level(users, self_user_id, users_default)?;
    let needed = [
        ("upgrade it", upgrade),
        ("lock it down", lock_down),
        ("move its aliases", move_aliases),
    ];
    if admin_api && needed.iter().any(|(_, required)| level < *required) {
        // Mirrors how make_room_admin picks the user to act as: local creators first, then the
        // local user with the most power, as long as they're joined. We get the level of that
        // user, or 100 for creators. Among users with the same level it may pick another one.
        let own_server = server_name(self_user_id)?;
        let mut candidates = users
            .iter()
            .map(|(user, level)| Ok((user.as_str(), parse_power_level(user, level)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        candidates.sort_by_key(|(_, level)| std::cmp::Reverse(*level));
        let candidate = creators
            .iter()
            .map(|creator| Ok((*creator, power_level(users, creator, int!(100))?)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .chain(candidates)
            .find(|(user, _)| {
                server_name(user).is_ok_and(|server| server == own_server)
                    && find("m.room.member", user)
                        .is_some_and(|event| event["content"]["membership"] == "join")
            });
        if let Some((admin_user, granted)) = candidate.filter(|(_, granted)| *granted > level) {
            let needed = needed
                .iter()
                .filter(|(_, required)| *required > int!(0))
                .map(|(step, required)| format!("{required} to {step}"))
                .collect::<Vec<_>>()
                .join(", ");
            if confirm(format!(
                "We have power level {level} in {room}, but need {needed}. Get power level \
                 {granted} through make_room_admin, acting as {admin_user}?"
            ))? {
                send(
                    http_client
                        .post(format!(
                            "{homeserver_url}/_synapse/admin/v1/rooms/{room}/make_room_admin"
                        ))
                        .json(&json!({})),
                )
                .await?;
                info!("Got power level {granted} in {room} through {admin_user}");
                level = granted;
            }
        }
    }

    anyhow::ensure!(
        level >= upgrade,
        "we have power level {level} in {room}, but need {upgrade} to upgrade it"
    );
    let do_step = |step: &str, required: Int| {
        if level >= required {
            return Ok(true);
        }
        anyhow::ensure!(
            confirm(format!(
                "We have power level {level} in {room}, but need {required} to {step}. \
                 Continue without it?"
            ))?,
            "we have power level {level} in {room}, but need {required} to {step}"
        );
        Ok(false)
    };
    let steps = Steps {
        lock_down: do_step("lock it down", lock_down)?,
        move_aliases: do_step("move its aliases", move_aliases)?,
    };
    join(http_client, homeserver_url, room).await?;
    Ok(steps)
}

/// Joins `room` unless we're in it already.
async fn join(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<()> {
    let joined_rooms =
        send(http_client.get(format!("{homeserver_url}/_matrix/client/v3/joined_rooms")))
            .await?
            .json::<Value>()
            .await?;
    if !joined_rooms["joined_rooms"]
        .as_array()
        .context("joined_rooms response has no joined_rooms")?
        .iter()
        .any(|joined_room| joined_room == room)
    {
        send(
            http_client
                .post(format!(
                    "{homeserver_url}/_matrix/client/v3/rooms/{room}/join"
                ))
                .json(&json!({})),
        )
        .await?;
        info!("Joined {room}");
    }
    Ok(())
}

/// Asks the user a yes/no question, defaulting to no.
fn confirm(prompt: String) -> anyhow::Result<bool> {
    Ok(dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?)
}

async fn upgrade_room(
    http_client: &reqwest::Client,
    config: &config::Config,
    state: &mut state::State,
    self_user_id: &str,
    room: &str,
    steps: Steps,
) -> anyhow::Result<()> {
    info!("Upgrading {room}");
    let tombstone = get_state(
        http_client,
        &config.homeserver_url,
        room,
        "m.room.tombstone",
        "",
    )
    .await?;
    let new_room_id = if let Some(tombstone_content) = tombstone {
        let new_room_id = tombstone_content["replacement_room"]
            .as_str()
            .context("tombstone has no replacement_room")?
            .to_string();
        info!("{room} was already upgraded to {new_room_id}, only transferring membership");
        Some(new_room_id)
    } else {
        None
    };

    let old_members_res = send(http_client.get(format!(
        "{}/_matrix/client/v3/rooms/{room}/members",
        config.homeserver_url
    )))
    .await?
    .json::<Value>()
    .await?;

    let mut banned_members: Vec<(String, Option<String>)> = Vec::new();
    let mut joined_members: Vec<(String, Option<String>)> = Vec::new();

    for member in old_members_res["chunk"]
        .as_array()
        .context("members response should have array called chunk but doesn't")?
        .iter()
    {
        let membership = member["content"]["membership"]
            .as_str()
            .context("member event has no membership")?;
        let entry = (
            member["state_key"]
                .as_str()
                .context("member event has no state_key")?
                .to_string(),
            member["content"]["reason"].as_str().map(str::to_string),
        );
        match membership {
            "join" | "invite" => joined_members.push(entry),
            "ban" => banned_members.push(entry),
            _ => {}
        }
    }

    debug!("Members in the old room: {joined_members:?}, banned: {banned_members:?}");

    let new_room_id = if let Some(new_room_id) = new_room_id {
        new_room_id
    } else {
        let new_room_id = if let Some(new_room_id) = state.replacement_rooms.get(room) {
            info!("Resuming the upgrade to {new_room_id}, which was created on a previous run");
            new_room_id.clone()
        } else {
            let new_room_id =
                create_replacement_room(http_client, config, self_user_id, room).await?;
            state
                .replacement_rooms
                .insert(room.to_string(), new_room_id.clone());
            state.save()?;
            info!("Created {new_room_id}");
            new_room_id
        };

        put_state(
            http_client,
            &config.homeserver_url,
            room,
            "m.room.tombstone",
            "",
            &json!({
                "body": "This room has been replaced",
                "replacement_room": new_room_id,
            }),
        )
        .await?;
        info!("Tombstoned {room}");
        // The tombstone records the new room from here on.
        if state.replacement_rooms.remove(room).is_some() {
            state.save()?;
        }
        new_room_id
    };
    if steps.lock_down {
        restrict_old_room(http_client, &config.homeserver_url, room).await?;
    } else {
        warn!("Not locking down {room}");
    }
    if steps.move_aliases {
        move_aliases(
            http_client,
            &config.homeserver_url,
            self_user_id,
            room,
            &new_room_id,
        )
        .await?;
    } else {
        warn!("Not moving the aliases of {room}");
    }
    let mut failures =
        move_space_parents(http_client, &config.homeserver_url, room, &new_room_id).await?;

    let new_members_res = send(http_client.get(format!(
        "{}/_matrix/client/v3/rooms/{new_room_id}/members",
        config.homeserver_url
    )))
    .await?
    .json::<Value>()
    .await?;
    let new_members: HashSet<&str> = new_members_res["chunk"]
        .as_array()
        .context("members response should have array called chunk but doesn't")?
        .iter()
        .filter_map(|member| member["state_key"].as_str())
        .collect();
    debug!("Members in the new room: {new_members:?}");

    for (user_id, reason) in banned_members.iter() {
        if new_members.contains(user_id.as_str()) {
            continue;
        }
        if let Err(err) = send(
            http_client
                .post(format!(
                    "{}/_matrix/client/v3/rooms/{new_room_id}/ban",
                    config.homeserver_url
                ))
                .json(&json!({
                    "reason": reason,
                    "user_id": user_id,
                })),
        )
        .await
        {
            warn!("Failed to ban {user_id}: {err:#}");
            failures += 1;
        } else {
            debug!("Banned {user_id}");
        }
    }

    for (user_id, reason) in joined_members.iter() {
        if config.drop_members.contains(user_id)
            || self_user_id == user_id
            || new_members.contains(user_id.as_str())
        {
            continue;
        }
        if let Err(err) = send(
            http_client
                .post(format!(
                    "{}/_matrix/client/v3/rooms/{new_room_id}/invite",
                    config.homeserver_url
                ))
                .json(&json!({
                    "reason": reason,
                    "user_id": user_id,
                })),
        )
        .await
        {
            warn!("Failed to invite {user_id}: {err:#}");
            failures += 1;
        } else {
            debug!("Invited {user_id}");
        }
    }
    anyhow::ensure!(
        failures == 0,
        "{failures} space updates, bans or invites failed, re-run to retry them"
    );
    Ok(())
}

/// Stops the old room from being used any further, like a server side upgrade does: raises the
/// power levels needed to send events and to invite to max(50, users_default + 1), and makes the
/// room invite only.
async fn restrict_old_room(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<()> {
    let mut power_levels = get_state(http_client, homeserver_url, room, "m.room.power_levels", "")
        .await?
        .context("room has no power levels")?;
    let map = power_levels
        .as_object_mut()
        .context("PL state is not an object")?;
    let restricted =
        int!(50).max(power_level(map, "users_default", int!(0))?.saturating_add(int!(1)));
    let mut changed = false;
    for key in ["events_default", "invite"] {
        if power_level(map, key, int!(0))? < restricted {
            map.insert(key.to_string(), json!(restricted));
            changed = true;
        }
    }
    if changed {
        put_state(
            http_client,
            homeserver_url,
            room,
            "m.room.power_levels",
            "",
            &power_levels,
        )
        .await?;
        info!("Raised the power level to send events and invite in {room} to {restricted}");
    }

    // A room without join rules is invite only already.
    let join_rules = get_state(http_client, homeserver_url, room, "m.room.join_rules", "").await?;
    if join_rules.is_some_and(|content| content["join_rule"] != "invite") {
        put_state(
            http_client,
            homeserver_url,
            room,
            "m.room.join_rules",
            "",
            &json!({ "join_rule": "invite" }),
        )
        .await?;
        info!("Made {room} invite only");
    }
    Ok(())
}

/// Moves the aliases, canonical alias and room directory listing of `room` to `new_room_id`.
///
/// Every step checks where things point now instead of assuming they haven't moved yet, so that
/// an interrupted run can be resumed: aliases on our server are made to point to the new room
/// whether they still point to the old one or were already deleted from it. Aliases on other
/// servers can't be moved and are dropped from the canonical alias unless they already point to
/// the new room.
async fn move_aliases(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    self_user_id: &str,
    room: &str,
    new_room_id: &str,
) -> anyhow::Result<()> {
    let server_name = server_name(self_user_id)?;
    let canonical_alias = get_state(
        http_client,
        homeserver_url,
        room,
        "m.room.canonical_alias",
        "",
    )
    .await?
    .filter(|content| content.as_object().is_some_and(|map| !map.is_empty()));

    let local_aliases = send(http_client.get(format!(
        "{homeserver_url}/_matrix/client/v3/rooms/{room}/aliases"
    )))
    .await?
    .json::<Value>()
    .await?;
    let mut aliases: HashSet<&str> = local_aliases["aliases"]
        .as_array()
        .context("aliases response has no aliases")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if let Some(canonical_alias) = &canonical_alias {
        aliases.extend(
            canonical_alias_entries(canonical_alias)
                .filter(|alias| alias.ends_with(&format!(":{server_name}"))),
        );
    }

    for alias in aliases {
        let url = directory_url(homeserver_url, alias)?;
        match resolve_alias(http_client, homeserver_url, alias).await? {
            Some(target) if target == new_room_id => continue,
            Some(target) if target == room => {
                send(http_client.delete(url.clone())).await?;
            }
            Some(target) => {
                warn!("{alias} points to {target} instead of {room}, leaving it alone");
                continue;
            }
            None => {}
        }
        send(
            http_client
                .put(url)
                .json(&json!({ "room_id": new_room_id })),
        )
        .await?;
        info!("Pointed {alias} to {new_room_id}");
    }

    if let Some(mut canonical_alias) = canonical_alias {
        let mut moved = HashSet::new();
        for alias in canonical_alias_entries(&canonical_alias) {
            if resolve_alias(http_client, homeserver_url, alias)
                .await?
                .as_deref()
                == Some(new_room_id)
            {
                moved.insert(alias.to_string());
            }
        }
        let map = canonical_alias
            .as_object_mut()
            .context("canonical alias is not an object")?;
        if map
            .get("alias")
            .and_then(Value::as_str)
            .is_some_and(|alias| !moved.contains(alias))
        {
            map.remove("alias");
        }
        if let Some(alt_aliases) = map.get_mut("alt_aliases").and_then(Value::as_array_mut) {
            alt_aliases.retain(|alias| alias.as_str().is_some_and(|alias| moved.contains(alias)));
        }
        put_state(
            http_client,
            homeserver_url,
            new_room_id,
            "m.room.canonical_alias",
            "",
            &canonical_alias,
        )
        .await?;
        put_state(
            http_client,
            homeserver_url,
            room,
            "m.room.canonical_alias",
            "",
            &json!({}),
        )
        .await?;
        info!("Moved the canonical alias of {room} to {new_room_id}");
    }

    let visibility = send(http_client.get(format!(
        "{homeserver_url}/_matrix/client/v3/directory/list/room/{room}"
    )))
    .await?
    .json::<Value>()
    .await?;
    if visibility["visibility"] == "public" {
        let set_visibility = |room: &str, visibility: &str| {
            send(
                http_client
                    .put(format!(
                        "{homeserver_url}/_matrix/client/v3/directory/list/room/{room}"
                    ))
                    .json(&json!({ "visibility": visibility })),
            )
        };
        // Servers can restrict who may publish rooms, which won't change by retrying, so this
        // doesn't fail the upgrade. The old room stays published to not drop out of the directory.
        if let Err(err) = set_visibility(new_room_id, "public").await {
            warn!("Failed to publish {new_room_id}, leaving {room} published instead: {err:#}");
        } else {
            set_visibility(room, "private").await?;
            info!("Replaced {room} with {new_room_id} in the room directory");
        }
    }
    Ok(())
}

/// Replaces `room` with `new_room_id` in the spaces that `room` names as its parents, and names
/// those spaces as parents of the new room. Returns how many spaces couldn't be updated, e.g.
/// because we lack the power level to change them.
async fn move_space_parents(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
) -> anyhow::Result<usize> {
    let state = send(http_client.get(format!(
        "{homeserver_url}/_matrix/client/v3/rooms/{room}/state"
    )))
    .await?
    .json::<Value>()
    .await?;
    let mut failures = 0;
    for event in state.as_array().context("state response is not an array")? {
        if event["type"] != "m.space.parent" || !has_via(&event["content"]) {
            continue;
        }
        let space = event["state_key"]
            .as_str()
            .context("space parent event has no state_key")?;
        if let Err(err) = move_space_parent(
            http_client,
            homeserver_url,
            room,
            new_room_id,
            space,
            &event["content"],
        )
        .await
        {
            warn!("Failed to replace {room} with {new_room_id} in {space}: {err:#}");
            failures += 1;
        }
    }
    Ok(failures)
}

/// Names `space` as a parent of `new_room_id` and, if `space` still lists `room` as a child,
/// replaces it with `new_room_id` there.
async fn move_space_parent(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
    space: &str,
    parent_content: &Value,
) -> anyhow::Result<()> {
    let new_parent_content = get_state(
        http_client,
        homeserver_url,
        new_room_id,
        "m.space.parent",
        space,
    )
    .await?;
    if new_parent_content.as_ref() != Some(parent_content) {
        put_state(
            http_client,
            homeserver_url,
            new_room_id,
            "m.space.parent",
            space,
            parent_content,
        )
        .await?;
    }

    let Some(child_content) = get_state(http_client, homeserver_url, space, "m.space.child", room)
        .await?
        .filter(has_via)
    else {
        return Ok(());
    };
    put_state(
        http_client,
        homeserver_url,
        space,
        "m.space.child",
        new_room_id,
        &child_content,
    )
    .await?;
    put_state(
        http_client,
        homeserver_url,
        space,
        "m.space.child",
        room,
        &json!({}),
    )
    .await?;
    info!("Replaced {room} with {new_room_id} in {space}");
    Ok(())
}

/// Lists the users with unlimited power in a room, given its `m.room.create` event. From room
/// version 12 on, these are its creators, who aren't in the power levels.
fn creators(create: &Value) -> Vec<&str> {
    if create["content"]["room_version"]
        .as_str()
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 12)
    {
        create["content"]["additional_creators"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(create["sender"].as_str())
            .collect()
    } else {
        Vec::new()
    }
}

/// Whether the content of an `m.space.child` or `m.space.parent` event lists servers in `via`,
/// without which the spec treats the relationship as nonexistent.
fn has_via(content: &Value) -> bool {
    content["via"].as_array().is_some_and(|via| !via.is_empty())
}

/// Lists the `alias` and `alt_aliases` in the content of an `m.room.canonical_alias` event.
fn canonical_alias_entries(content: &Value) -> impl Iterator<Item = &str> {
    content["alias"].as_str().into_iter().chain(
        content["alt_aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    )
}

/// Looks up the room an alias points to, or `None` if the alias doesn't exist.
async fn resolve_alias(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    alias: &str,
) -> anyhow::Result<Option<String>> {
    let res = send_retrying(http_client.get(directory_url(homeserver_url, alias)?)).await?;
    if res.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body = error_for_status(res).await?.json::<Value>().await?;
    Ok(Some(
        body["room_id"]
            .as_str()
            .context("alias response has no room_id")?
            .to_string(),
    ))
}

/// Builds the room directory URL for `alias`, escaping the `#` it starts with.
fn directory_url(homeserver_url: &str, alias: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!(
        "{homeserver_url}/_matrix/client/v3/directory/room"
    ))?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("{homeserver_url} is not a valid base URL"))?
        .push(alias);
    Ok(url)
}

/// Reads a power level from `power_levels`, which is the content of an `m.room.power_levels`
/// event or one of the maps in it, falling back to `default` if it's missing.
fn power_level(
    power_levels: &serde_json::Map<String, Value>,
    key: &str,
    default: Int,
) -> anyhow::Result<Int> {
    match power_levels.get(key) {
        Some(level) => parse_power_level(key, level),
        None => Ok(default),
    }
}

/// Parses the power level stored under `key`, which room versions before 10 also allow to be a
/// string.
fn parse_power_level(key: &str, level: &Value) -> anyhow::Result<Int> {
    match level {
        Value::String(level) => level.parse().map_err(anyhow::Error::from),
        level => Int::deserialize(level).map_err(anyhow::Error::from),
    }
    .with_context(|| format!("PL state key {key} is not an integer"))
}

/// Rewrites the power levels in the content of an `m.room.power_levels` event that are strings
/// as integers, which room version 10 and later require.
fn normalize_power_levels(power_levels: &mut serde_json::Map<String, Value>) -> anyhow::Result<()> {
    for (key, value) in power_levels.iter_mut() {
        if let Value::Object(levels) = value {
            for (key, level) in levels.iter_mut() {
                *level = json!(parse_power_level(key, level)?);
            }
        } else {
            *value = json!(parse_power_level(key, value)?);
        }
    }
    Ok(())
}

/// Extracts the server name from a user ID.
fn server_name(user_id: &str) -> anyhow::Result<&str> {
    Ok(user_id
        .split_once(':')
        .with_context(|| format!("{user_id} has no server name"))?
        .1)
}

/// Sends a state event.
async fn put_state(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    event_type: &str,
    state_key: &str,
    content: &Value,
) -> anyhow::Result<()> {
    send(
        http_client
            .put(format!(
                "{homeserver_url}/_matrix/client/v3/rooms/{room}/state/{event_type}/{state_key}"
            ))
            .json(content),
    )
    .await?;
    Ok(())
}

/// Fetches the content of a state event, or `None` if the room has no such state event.
async fn get_state(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    event_type: &str,
    state_key: &str,
) -> anyhow::Result<Option<Value>> {
    let res = send_retrying(http_client.get(format!(
        "{homeserver_url}/_matrix/client/v3/rooms/{room}/state/{event_type}/{state_key}"
    )))
    .await?;
    if res.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(error_for_status(res).await?.json().await?))
}

/// Sends a request like `send_retrying`, failing on any non-success response.
async fn send(request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    error_for_status(send_retrying(request).await?).await
}

/// Turns a non-success response into an error carrying the response body, which for Matrix
/// errors holds the `errcode` and `error`.
async fn error_for_status(res: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let url = res.url().clone();
    let body = res.text().await?;
    anyhow::bail!("{url} returned {status}: {body}")
}

/// Sends a request, retrying while the server rate limits us. Waits as long as the server asks
/// via `Retry-After` or `retry_after_ms`, otherwise backs off exponentially up to `MAX_BACKOFF`.
async fn send_retrying(request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let (client, request) = request.build_split();
    let request = request?;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let res = client
            .execute(request.try_clone().context("request should be cloneable")?)
            .await?;
        if res.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(res);
        }
        let retry_after = res
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()?.parse().ok())
            .map(Duration::from_secs);
        let retry_after = match retry_after {
            Some(retry_after) => Some(retry_after),
            None => res
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| body["retry_after_ms"].as_u64())
                .map(Duration::from_millis),
        };
        let wait = retry_after.unwrap_or(backoff);
        warn!("Rate limited, retrying in {wait:?}");
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
