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
const CLIENT_API: &str = "_matrix/client/v3";
const ADMIN_API: &str = "_synapse/admin/v1";
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

    let self_user_id_res = send(http_client.get(url(
        &config.homeserver_url,
        CLIENT_API,
        &["account", "whoami"],
    )?))
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

/// Posts an upgrade notice in `room`, unless a previous run did already, and creates the room
/// replacing it, returning its ID.
async fn create_replacement_room(
    http_client: &reqwest::Client,
    config: &config::Config,
    state: &mut state::State,
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
    let room_state = room_state(http_client, &config.homeserver_url, room).await?;
    let create = room_state
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
    // Spaces keep their children, and rooms the spaces they name as their parents.
    initial_state.extend(
        room_state
            .iter()
            .filter(|event| {
                (event["type"] == "m.space.child" || event["type"] == "m.space.parent")
                    && has_via(&event["content"])
            })
            .map(|event| {
                json!({
                    "content": event["content"],
                    "state_key": event["state_key"],
                    "type": event["type"],
                })
            }),
    );
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

    let last_event_id = if let Some(event_id) = state.upgrade_notices.get(room) {
        event_id.clone()
    } else {
        let txn_id = Uuid::new_v4();
        let res = send(
            http_client
                .put(url(
                    &config.homeserver_url,
                    CLIENT_API,
                    &["rooms", room, "send", "m.room.message", &txn_id.to_string()],
                )?)
                .json(&json!({
                    "body": "Upgrading room, please stand by",
                    "msgtype": "m.text"
                }
                )),
        )
        .await?;
        let event_id = res.json::<Value>().await?["event_id"]
            .as_str()
            .context("event_id is not a string")?
            .to_string();
        state
            .upgrade_notices
            .insert(room.to_string(), event_id.clone());
        state.save()?;
        event_id
    };

    debug!("Last event ID: {last_event_id}");

    let target_room_version = format!("{}", config.target_room_version);

    // The new room keeps the rest of the old create event, like the type that makes a room a space,
    // or m.federate. Its creators get power through the power levels instead.
    let mut creation_content = create["content"]
        .as_object()
        .context("create event content is not an object")?
        .clone();
    for key in ["additional_creators", "creator", "room_version"] {
        creation_content.remove(key);
    }
    creation_content.insert(
        "predecessor".to_string(),
        json!({
            "event_id": last_event_id,
            "room_id": room,
        }),
    );

    let new_room_body = send(
        http_client
            .post(url(&config.homeserver_url, CLIENT_API, &["createRoom"])?)
            .json(&json!({
                "creation_content": creation_content,
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
    let (admin_api, room_state) = match admin_room_state(http_client, homeserver_url, room).await? {
        Some(room_state) => (true, room_state),
        None => {
            join(http_client, homeserver_url, room).await?;
            (false, room_state(http_client, homeserver_url, room).await?)
        }
    };
    let power = Power::new(&room_state)?;

    // The power needed for each step, counting only changes that are still to be made.
    let upgrade = if power.find("m.room.tombstone", "").is_some() {
        int!(0)
    } else {
        power
            .state_event("m.room.tombstone")?
            .max(power.event("m.room.message")?)
    };
    let lock_down = power.lock_down()?;
    let mut has_aliases = power
        .find("m.room.canonical_alias", "")
        .is_some_and(|event| {
            event["content"]
                .as_object()
                .is_some_and(|content| !content.is_empty())
        });
    // Deleting aliases we didn't create and changing the room directory need the power to change
    // the canonical alias too, unless we're a server admin.
    if !has_aliases && !admin_api {
        has_aliases = !local_aliases(http_client, homeserver_url, room)
            .await?
            .is_empty()
            || is_published(http_client, homeserver_url, room).await?;
    }
    let move_aliases = if has_aliases {
        power.state_event("m.room.canonical_alias")?
    } else {
        int!(0)
    };

    let mut level = power.user(self_user_id)?;
    let needed = [
        ("upgrade it", upgrade),
        ("lock it down", lock_down),
        ("move its aliases", move_aliases),
    ];
    if admin_api && needed.iter().any(|(_, required)| level < *required) {
        let needed = needed
            .iter()
            .filter(|(_, required)| *required > int!(0))
            .map(|(step, required)| format!("{required} to {step}"))
            .collect::<Vec<_>>()
            .join(", ");
        level = offer_make_room_admin(
            http_client,
            homeserver_url,
            self_user_id,
            room,
            &power,
            level,
            &needed,
        )
        .await?;
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

/// Fetches the state of `room` through the admin API, which also works for rooms we aren't in, or
/// returns `None` if we can't use the admin API. Only server admins can use it, and reverse
/// proxies often don't expose it.
async fn admin_room_state(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<Option<Vec<Value>>> {
    let res =
        send_retrying(http_client.get(url(homeserver_url, ADMIN_API, &["rooms", room, "state"])?))
            .await?;
    if matches!(res.status(), StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        return Ok(None);
    }
    let mut body = error_for_status(res).await?.json::<Value>().await?;
    Ok(Some(serde_json::from_value(body["state"].take())?))
}

/// Fetches the state of `room`, which we need to be in.
async fn room_state(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<Vec<Value>> {
    Ok(
        send(http_client.get(url(homeserver_url, CLIENT_API, &["rooms", room, "state"])?))
            .await?
            .json()
            .await?,
    )
}

/// Who has how much power in a room, and how much power doing things there needs, going by the
/// room's state.
struct Power<'a> {
    room_state: &'a [Value],
    creators: Vec<&'a str>,
    /// The content of the room's power levels, without which everyone may do anything.
    power_levels: Option<&'a serde_json::Map<String, Value>>,
}

impl<'a> Power<'a> {
    fn new(room_state: &'a [Value]) -> anyhow::Result<Self> {
        let find = |event_type: &str| {
            room_state
                .iter()
                .find(|event| event["type"] == event_type && event["state_key"] == "")
        };
        let power_levels = find("m.room.power_levels")
            .map(|event| {
                event["content"]
                    .as_object()
                    .context("PL state is not an object")
            })
            .transpose()?;
        Ok(Self {
            room_state,
            creators: creators(find("m.room.create").context("room has no create event")?),
            power_levels,
        })
    }

    /// Finds the state event of `event_type` with `state_key`.
    fn find(&self, event_type: &str, state_key: &str) -> Option<&'a Value> {
        self.room_state
            .iter()
            .find(|event| event["type"] == event_type && event["state_key"] == state_key)
    }

    /// Reads the power level stored under `key`, falling back to `default` if it's missing.
    fn level(&self, key: &str, default: Int) -> anyhow::Result<Int> {
        match self.power_levels {
            Some(power_levels) => power_level(power_levels, key, default),
            None => Ok(int!(0)),
        }
    }

    /// Reads the power level stored under `key` in the map of power levels stored under `map`,
    /// falling back to `default` if it's missing.
    fn map_level(&self, map: &str, key: &str, default: Int) -> anyhow::Result<Int> {
        match self.map(map)?.and_then(|map| map.get(key)) {
            Some(level) => parse_power_level(key, level),
            None => Ok(default),
        }
    }

    /// The map of power levels stored under `key`.
    fn map(&self, key: &str) -> anyhow::Result<Option<&'a serde_json::Map<String, Value>>> {
        self.power_levels
            .and_then(|power_levels| power_levels.get(key))
            .map(|map| {
                map.as_object()
                    .with_context(|| format!("PL state key {key} is not an object"))
            })
            .transpose()
    }

    /// The power level of `user`.
    fn user(&self, user: &str) -> anyhow::Result<Int> {
        if self.creators.contains(&user) {
            return Ok(Int::MAX);
        }
        self.map_level("users", user, self.level("users_default", int!(0))?)
    }

    /// The power level needed to send state events of `event_type`.
    fn state_event(&self, event_type: &str) -> anyhow::Result<Int> {
        self.map_level("events", event_type, self.level("state_default", int!(50))?)
    }

    /// The power level needed to send events of `event_type` that aren't state events.
    fn event(&self, event_type: &str) -> anyhow::Result<Int> {
        self.map_level("events", event_type, self.level("events_default", int!(0))?)
    }

    /// The power level needed to lock the room down like `restrict_old_room` does, or 0 if it's
    /// locked down already.
    fn lock_down(&self) -> anyhow::Result<Int> {
        let mut needed = int!(0);
        let restricted = int!(50).max(
            self.level("users_default", int!(0))?
                .saturating_add(int!(1)),
        );
        if self.level("events_default", int!(0))? < restricted
            || self.level("invite", int!(0))? < restricted
        {
            needed = restricted.max(self.state_event("m.room.power_levels")?);
        }
        if self
            .find("m.room.join_rules", "")
            .is_some_and(|event| event["content"]["join_rule"] != "invite")
        {
            needed = needed.max(self.state_event("m.room.join_rules")?);
        }
        Ok(needed)
    }

    /// The user make_room_admin would act as when we use it, and the power level we would get.
    ///
    /// Mirrors how make_room_admin picks the user: local creators first, then the local user with
    /// the most power, as long as they're joined. We get the level of that user, or 100 for
    /// creators. Among users with the same level it may pick another one.
    fn make_room_admin_candidate(
        &self,
        self_user_id: &str,
    ) -> anyhow::Result<Option<(String, Int)>> {
        let own_server = server_name(self_user_id)?;
        let mut candidates = self
            .map("users")?
            .into_iter()
            .flatten()
            .map(|(user, level)| Ok((user.as_str(), parse_power_level(user, level)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        candidates.sort_by_key(|(_, level)| std::cmp::Reverse(*level));
        Ok(self
            .creators
            .iter()
            .map(|creator| Ok((*creator, self.map_level("users", creator, int!(100))?)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .chain(candidates)
            .find(|(user, _)| {
                server_name(user).is_ok_and(|server| server == own_server)
                    && self
                        .find("m.room.member", user)
                        .is_some_and(|event| event["content"]["membership"] == "join")
            })
            .map(|(user, level)| (user.to_string(), level)))
    }
}

/// Offers to take over the power of another user in `room` through Synapse's make_room_admin
/// admin API, if that gets us more than `level`, which is too little to do what `needed`
/// describes. Returns the power level we have afterwards.
async fn offer_make_room_admin(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    self_user_id: &str,
    room: &str,
    power: &Power<'_>,
    level: Int,
    needed: &str,
) -> anyhow::Result<Int> {
    let Some((admin_user, granted)) = power
        .make_room_admin_candidate(self_user_id)?
        .filter(|(_, granted)| *granted > level)
    else {
        return Ok(level);
    };
    if !confirm(format!(
        "We have power level {level} in {room}, but need {needed}. Get power level {granted} \
         through make_room_admin, acting as {admin_user}?"
    ))? {
        return Ok(level);
    }
    send(
        http_client
            .post(url(
                homeserver_url,
                ADMIN_API,
                &["rooms", room, "make_room_admin"],
            )?)
            .json(&json!({})),
    )
    .await?;
    info!("Got power level {granted} in {room} through {admin_user}");
    Ok(granted)
}

/// Joins `room` unless we're in it already.
async fn join(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<()> {
    if !joined_rooms(http_client, homeserver_url)
        .await?
        .iter()
        .any(|joined_room| joined_room == room)
    {
        send(
            http_client
                .post(url(homeserver_url, CLIENT_API, &["rooms", room, "join"])?)
                .json(&json!({})),
        )
        .await?;
        info!("Joined {room}");
    }
    Ok(())
}

/// Lists the rooms we're in.
async fn joined_rooms(
    http_client: &reqwest::Client,
    homeserver_url: &str,
) -> anyhow::Result<Vec<String>> {
    let res = send(http_client.get(url(homeserver_url, CLIENT_API, &["joined_rooms"])?))
        .await?
        .json::<Value>()
        .await?;
    Ok(res["joined_rooms"]
        .as_array()
        .context("joined_rooms response has no joined_rooms")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
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
        // Someone else may have upgraded the room.
        join(http_client, &config.homeserver_url, &new_room_id).await?;
        Some(new_room_id)
    } else {
        None
    };

    let old_members_res = send(http_client.get(url(
        &config.homeserver_url,
        CLIENT_API,
        &["rooms", room, "members"],
    )?))
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
            // The reason given for joining is the joining user's own, not one to invite them with.
            member["content"]["reason"]
                .as_str()
                .filter(|_| membership != "join")
                .map(str::to_string),
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
                create_replacement_room(http_client, config, state, self_user_id, room).await?;
            state
                .replacement_rooms
                .insert(room.to_string(), new_room_id.clone());
            state.upgrade_notices.remove(room);
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
    let mut failures = 0;
    if !steps.lock_down {
        warn!("Not locking down {room}");
    } else if let Err(err) = restrict_old_room(http_client, &config.homeserver_url, room).await {
        warn!("Failed to lock down {room}: {err:#}");
        failures += 1;
    }
    if !steps.move_aliases {
        warn!("Not moving the aliases of {room}");
    } else if let Err(err) = move_aliases(
        http_client,
        &config.homeserver_url,
        state,
        self_user_id,
        room,
        &new_room_id,
    )
    .await
    {
        warn!("Failed to move the aliases of {room}: {err:#}");
        failures += 1;
    }
    failures += move_references(http_client, &config.homeserver_url, room, &new_room_id).await?;

    let new_members_res = send(http_client.get(url(
        &config.homeserver_url,
        CLIENT_API,
        &["rooms", &new_room_id, "members"],
    )?))
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
                .post(url(
                    &config.homeserver_url,
                    CLIENT_API,
                    &["rooms", &new_room_id, "ban"],
                )?)
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
                .post(url(
                    &config.homeserver_url,
                    CLIENT_API,
                    &["rooms", &new_room_id, "invite"],
                )?)
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
        "{failures} steps of the upgrade failed, re-run to retry them"
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
/// whether they still point to the old one or were already deleted from it, which `state`
/// remembers for aliases that aren't in the canonical alias. Aliases on other servers can't be
/// moved and are dropped from the canonical alias unless they already point to the new room.
async fn move_aliases(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    state: &mut state::State,
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

    let mut aliases: HashSet<String> = local_aliases(http_client, homeserver_url, room)
        .await?
        .into_iter()
        .collect();
    if let Some(canonical_alias) = &canonical_alias {
        aliases.extend(
            canonical_alias_entries(canonical_alias)
                .filter(|alias| alias.ends_with(&format!(":{server_name}")))
                .map(str::to_string),
        );
    }
    aliases.extend(
        state
            .moving_aliases
            .iter()
            .filter(|(_, old_room)| *old_room == room)
            .map(|(alias, _)| alias.clone()),
    );

    for alias in &aliases {
        let alias_url = url(homeserver_url, CLIENT_API, &["directory", "room", alias])?;
        match resolve_alias(http_client, homeserver_url, alias).await? {
            Some(target) if target == new_room_id => {}
            Some(target) if target != room => {
                warn!("{alias} points to {target} instead of {room}, leaving it alone");
            }
            target => {
                if target.is_some() {
                    state.moving_aliases.insert(alias.clone(), room.to_string());
                    state.save()?;
                    send(http_client.delete(alias_url.clone())).await?;
                }
                send(
                    http_client
                        .put(alias_url)
                        .json(&json!({ "room_id": new_room_id })),
                )
                .await?;
                info!("Pointed {alias} to {new_room_id}");
            }
        }
        if state.moving_aliases.remove(alias).is_some() {
            state.save()?;
        }
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

    if is_published(http_client, homeserver_url, room).await? {
        let set_visibility = |room: &str, visibility: &str| {
            anyhow::Ok(send(
                http_client
                    .put(url(
                        homeserver_url,
                        CLIENT_API,
                        &["directory", "list", "room", room],
                    )?)
                    .json(&json!({ "visibility": visibility })),
            ))
        };
        // Servers can restrict who may publish rooms, which won't change by retrying, so this
        // doesn't fail the upgrade. The old room stays published to not drop out of the directory.
        if let Err(err) = set_visibility(new_room_id, "public")?.await {
            warn!("Failed to publish {new_room_id}, leaving {room} published instead: {err:#}");
        } else {
            set_visibility(room, "private")?.await?;
            info!("Replaced {room} with {new_room_id} in the room directory");
        }
    }
    Ok(())
}

/// Updates the rooms we're in that refer to `room` to refer to `new_room_id`. Returns how many
/// updates failed, e.g. because we lack the power level to make them.
async fn move_references(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
) -> anyhow::Result<usize> {
    let mut failures = 0;
    for other_room in joined_rooms(http_client, homeserver_url).await? {
        if let Err(err) =
            move_join_rule_reference(http_client, homeserver_url, room, new_room_id, &other_room)
                .await
        {
            warn!("Failed to allow members of {new_room_id} to join {other_room}: {err:#}");
            failures += 1;
        }
        if let Err(err) =
            move_space_parent_reference(http_client, homeserver_url, room, new_room_id, &other_room)
                .await
        {
            warn!("Failed to make {new_room_id} a parent of {other_room}: {err:#}");
            failures += 1;
        }
        if let Err(err) =
            move_space_child_reference(http_client, homeserver_url, room, new_room_id, &other_room)
                .await
        {
            warn!("Failed to replace {room} with {new_room_id} in {other_room}: {err:#}");
            failures += 1;
        }
    }
    Ok(failures)
}

/// Lets members of `new_room_id` join `other_room` if its join rules let members of `room` join.
/// Members of `room` stay allowed, as not all of them will have moved yet.
async fn move_join_rule_reference(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
    other_room: &str,
) -> anyhow::Result<()> {
    let Some(mut join_rules) = get_state(
        http_client,
        homeserver_url,
        other_room,
        "m.room.join_rules",
        "",
    )
    .await?
    else {
        return Ok(());
    };
    let Some(allow) = join_rules["allow"].as_array_mut() else {
        return Ok(());
    };
    let allows = |room_id: &str| {
        allow.iter().any(|condition| {
            condition["type"] == "m.room_membership" && condition["room_id"] == room_id
        })
    };
    if !allows(room) || allows(new_room_id) {
        return Ok(());
    }
    allow.push(json!({ "type": "m.room_membership", "room_id": new_room_id }));
    put_state(
        http_client,
        homeserver_url,
        other_room,
        "m.room.join_rules",
        "",
        &join_rules,
    )
    .await?;
    info!("Allowed members of {new_room_id} to join {other_room}");
    Ok(())
}

/// Replaces `room` with `new_room_id` as a parent space of `other_room`, if it is one.
async fn move_space_parent_reference(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
    other_room: &str,
) -> anyhow::Result<()> {
    let Some(content) = get_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.parent",
        room,
    )
    .await?
    .filter(has_via) else {
        return Ok(());
    };
    put_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.parent",
        new_room_id,
        &content,
    )
    .await?;
    put_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.parent",
        room,
        &json!({}),
    )
    .await?;
    info!("Replaced {room} with {new_room_id} as a parent of {other_room}");
    Ok(())
}

/// Replaces `room` with `new_room_id` as a child of `other_room`, if it is one.
async fn move_space_child_reference(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    new_room_id: &str,
    other_room: &str,
) -> anyhow::Result<()> {
    let Some(content) = get_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.child",
        room,
    )
    .await?
    .filter(has_via) else {
        return Ok(());
    };
    put_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.child",
        new_room_id,
        &content,
    )
    .await?;
    put_state(
        http_client,
        homeserver_url,
        other_room,
        "m.space.child",
        room,
        &json!({}),
    )
    .await?;
    info!("Replaced {room} with {new_room_id} in {other_room}");
    Ok(())
}

/// Lists the aliases our server has for `room`.
async fn local_aliases(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<Vec<String>> {
    let res = send(http_client.get(url(
        homeserver_url,
        CLIENT_API,
        &["rooms", room, "aliases"],
    )?))
    .await?
    .json::<Value>()
    .await?;
    Ok(res["aliases"]
        .as_array()
        .context("aliases response has no aliases")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}

/// Whether `room` is published in the room directory.
async fn is_published(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<bool> {
    let res = send(http_client.get(url(
        homeserver_url,
        CLIENT_API,
        &["directory", "list", "room", room],
    )?))
    .await?
    .json::<Value>()
    .await?;
    Ok(res["visibility"] == "public")
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
    let res = send_retrying(http_client.get(url(
        homeserver_url,
        CLIENT_API,
        &["directory", "room", alias],
    )?))
    .await?;
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

/// Builds the URL of the endpoint at `path` in `api`, escaping each segment of `path`, like the
/// `#` that aliases start with.
fn url(homeserver_url: &str, api: &str, path: &[&str]) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{}/{api}", homeserver_url.trim_end_matches('/')))?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("{homeserver_url} is not a valid base URL"))?
        .extend(path);
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
            .put(url(
                homeserver_url,
                CLIENT_API,
                &["rooms", room, "state", event_type, state_key],
            )?)
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
    let res = send_retrying(http_client.get(url(
        homeserver_url,
        CLIENT_API,
        &["rooms", room, "state", event_type, state_key],
    )?))
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
