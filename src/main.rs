use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Whether to only log what would be done, as set on the command line.
static DRY_RUN: AtomicBool = AtomicBool::new(false);

fn dry_run() -> bool {
    DRY_RUN.load(Ordering::Relaxed)
}

mod cli;
mod config;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    DRY_RUN.store(cli.dry_run, Ordering::Relaxed);
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
    let mut failures = Failures::default();
    let mut prepared_rooms = Vec::new();
    for room in &config.rooms {
        match prepare_room(&http_client, &config, &self_user_id, room).await {
            Ok(Some(steps)) => prepared_rooms.push((room, steps)),
            Ok(None) => {}
            Err(err) => {
                error!("Failed to prepare {room}: {err:#}");
                failures.add("preparing rooms");
            }
        }
    }
    let fixes = plan_fixes(&http_client, &config, &self_user_id, &mut failures).await?;
    if !dry_run() {
        for (room, steps) in prepared_rooms {
            if let Err(err) = upgrade_room(
                &http_client,
                &config,
                &mut state,
                &self_user_id,
                room,
                steps,
                &mut failures,
            )
            .await
            {
                error!("Failed to upgrade {room}: {err:#}");
                failures.add("upgrading rooms");
            }
        }
        apply_fixes(&http_client, &config, &self_user_id, fixes, &mut failures).await?;
    }
    anyhow::ensure!(
        failures.0.is_empty(),
        "some steps failed, see the warnings above and re-run to retry them: {failures}"
    );
    Ok(())
}

/// How often each kind of step failed, to sum them up at the end.
#[derive(Default)]
struct Failures(Vec<(&'static str, usize)>);

impl Failures {
    /// Counts a failure of the step `kind` describes, like "inviting users".
    fn add(&mut self, kind: &'static str) {
        match self.0.iter_mut().find(|(other, _)| *other == kind) {
            Some((_, count)) => *count += 1,
            None => self.0.push((kind, 1)),
        }
    }
}

impl fmt::Display for Failures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<_> = self
            .0
            .iter()
            .map(|(kind, count)| format!("{kind} ({count})"))
            .collect();
        f.write_str(&kinds.join(", "))
    }
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
///
/// In a dry run, this returns `None` for rooms it can't look into without joining them.
async fn prepare_room(
    http_client: &reqwest::Client,
    config: &config::Config,
    self_user_id: &str,
    room: &str,
) -> anyhow::Result<Option<Steps>> {
    let homeserver_url = &config.homeserver_url;
    let (admin_api, room_state) = match admin_room_state(http_client, homeserver_url, room).await? {
        Some(room_state) => (true, room_state),
        None => {
            if !join(http_client, homeserver_url, room).await? {
                warn!("Can't check {room} without joining it or using the admin API");
                return Ok(None);
            }
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
    let mut has_aliases = power.has_canonical_alias();
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
    if dry_run() {
        info!(
            "{}",
            describe_upgrade(
                config,
                self_user_id,
                room,
                &power,
                steps.lock_down && lock_down > int!(0),
                steps.move_aliases && has_aliases,
            )
        );
    }
    Ok(Some(steps))
}

/// Describes what upgrading `room` would do, for dry runs.
fn describe_upgrade(
    config: &config::Config,
    self_user_id: &str,
    room: &str,
    power: &Power<'_>,
    lock_down: bool,
    move_aliases: bool,
) -> String {
    let mut steps = Vec::new();
    match power
        .find("m.room.tombstone", "")
        .and_then(|tombstone| tombstone["content"]["replacement_room"].as_str())
    {
        Some(replacement) => steps.push(format!("finish upgrading it to {replacement}")),
        None => {
            let transferred: Vec<&str> = config
                .state_events_to_transfer
                .iter()
                .filter(|event_type| power.find(event_type, "").is_some())
                .map(String::as_str)
                .collect();
            steps.push(format!(
                "create a room of version {} with its {}",
                config.target_room_version,
                transferred.join(", ")
            ));
        }
    }
    if lock_down {
        steps.push("lock it down".to_string());
    }
    if move_aliases {
        steps.push("move its aliases".to_string());
    }
    let (mut invites, mut bans) = (0, 0);
    for event in power.room_state {
        let Some(user_id) = event["state_key"].as_str() else {
            continue;
        };
        if event["type"] != "m.room.member"
            || user_id == self_user_id
            || config.drop_members.iter().any(|dropped| dropped == user_id)
        {
            continue;
        }
        match event["content"]["membership"].as_str() {
            Some("join" | "invite") => invites += 1,
            Some("ban") => bans += 1,
            _ => {}
        }
    }
    steps.push(format!("invite {invites} and ban {bans} members"));
    format!("Would upgrade {room}: {}", steps.join(", "))
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

/// Whether we can use the admin API. Only server admins can use it, and reverse proxies often
/// don't expose it.
async fn admin_api_available(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    self_user_id: &str,
) -> anyhow::Result<bool> {
    let res = send_retrying(http_client.get(url(
        homeserver_url,
        ADMIN_API,
        &["users", self_user_id, "admin"],
    )?))
    .await?;
    if matches!(res.status(), StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        return Ok(false);
    }
    error_for_status(res).await?;
    Ok(true)
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

    /// Whether the room has a canonical alias.
    fn has_canonical_alias(&self) -> bool {
        self.find("m.room.canonical_alias", "")
            .is_some_and(|event| {
                event["content"]
                    .as_object()
                    .is_some_and(|content| !content.is_empty())
            })
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
    if dry_run() {
        info!("Would get power level {granted} in {room} through {admin_user}");
        return Ok(granted);
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

/// Joins `room` unless we're in it already, returning whether we are in it now, which in a dry
/// run we are only if we were already.
async fn join(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
) -> anyhow::Result<bool> {
    if !joined_rooms(http_client, homeserver_url)
        .await?
        .iter()
        .any(|joined_room| joined_room == room)
    {
        if dry_run() {
            info!("Would join {room}");
            return Ok(false);
        }
        send(
            http_client
                .post(url(homeserver_url, CLIENT_API, &["rooms", room, "join"])?)
                .json(&json!({})),
        )
        .await?;
        info!("Joined {room}");
    }
    Ok(true)
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

/// Asks the user a yes/no question, defaulting to no. In a dry run, this only logs the question
/// and answers yes, to show what would be done then.
fn confirm(prompt: String) -> anyhow::Result<bool> {
    if dry_run() {
        info!("Would ask: {prompt}");
        return Ok(true);
    }
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
    failures: &mut Failures,
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
    if !steps.lock_down {
        warn!("Not locking down {room}");
    } else if let Err(err) = restrict_old_room(http_client, &config.homeserver_url, room).await {
        warn!("Failed to lock down {room}: {err:#}");
        failures.add("locking down rooms");
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
        failures.add("moving aliases");
    }

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
            failures.add("banning users");
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
            failures.add("inviting users");
        } else {
            debug!("Invited {user_id}");
        }
    }
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

/// Adds the replacement room IDs of the rooms in `room_states` that were upgraded to
/// `replacements`, by the ID of the room they replace.
fn add_replacements(
    replacements: &mut HashMap<String, String>,
    room_states: &[(String, Vec<Value>)],
) {
    for (room, room_state) in room_states {
        let replacement = room_state
            .iter()
            .find(|event| event["type"] == "m.room.tombstone" && event["state_key"] == "")
            .and_then(|tombstone| tombstone["content"]["replacement_room"].as_str());
        if let Some(replacement) = replacement {
            replacements.insert(room.clone(), replacement.to_string());
        }
    }
}

/// Follows the upgrades of `room` in `replacements`, returning the rooms that replaced it in
/// order, which ends with the one that is current.
fn replacement_chain<'a>(replacements: &'a HashMap<String, String>, room: &'a str) -> Vec<&'a str> {
    let mut chain = Vec::new();
    let mut room = room;
    while let Some(replacement) = replacements.get(room) {
        // Rooms can't be upgraded to rooms they were upgraded from, but tombstones can say so.
        if replacement == room || chain.contains(&replacement.as_str()) {
            break;
        }
        chain.push(replacement.as_str());
        room = replacement;
    }
    chain
}

/// A change to a room's state that makes it refer to the current replacements of upgraded rooms
/// instead of the upgraded rooms.
struct Change {
    /// What the change does, phrased as an instruction.
    description: String,
    /// The upgraded rooms the change stops referring to.
    upgraded_rooms: Vec<String>,
    /// The state events to send, as their type, state key and content.
    events: Vec<(&'static str, String, Value)>,
}

/// Lists the changes that make `room_state` refer to the current replacements of upgraded rooms,
/// for the upgraded rooms that `fix` returns true for.
fn outdated_references(
    room: &str,
    room_state: &[Value],
    replacements: &HashMap<String, String>,
    fix: impl Fn(&str) -> bool,
) -> Vec<Change> {
    let latest = |upgraded_room: &str| {
        let (upgraded_room, _) = replacements.get_key_value(upgraded_room)?;
        if !fix(upgraded_room) {
            return None;
        }
        replacement_chain(replacements, upgraded_room)
            .last()
            .copied()
    };
    let mut changes = Vec::new();

    let join_rules = room_state
        .iter()
        .find(|event| event["type"] == "m.room.join_rules" && event["state_key"] == "")
        .map(|event| &event["content"]);
    if let Some((join_rules, allow)) =
        join_rules.and_then(|content| Some((content, content["allow"].as_array()?)))
    {
        let mut upgraded_rooms = Vec::new();
        let mut replaced = Vec::new();
        let mut new_allow: Vec<Value> = Vec::new();
        for condition in allow {
            let mut condition = condition.clone();
            if condition["type"] == "m.room_membership" {
                if let Some((old, new)) = condition["room_id"]
                    .as_str()
                    .and_then(|old| Some((old.to_string(), latest(old)?)))
                {
                    replaced.push(format!("{old} with {new}"));
                    upgraded_rooms.push(old);
                    condition["room_id"] = json!(new);
                }
            }
            if !new_allow.contains(&condition) {
                new_allow.push(condition);
            }
        }
        if !replaced.is_empty() {
            let mut content = join_rules.clone();
            content["allow"] = json!(new_allow);
            changes.push(Change {
                description: format!(
                    "Replace {} in the join rules of {room}",
                    replaced.join(", ")
                ),
                upgraded_rooms,
                events: vec![("m.room.join_rules", String::new(), content)],
            });
        }
    }

    for event in room_state {
        let event_type = match event["type"].as_str() {
            Some("m.space.parent") => "m.space.parent",
            Some("m.space.child") => "m.space.child",
            _ => continue,
        };
        let (Some(old), true) = (event["state_key"].as_str(), has_via(&event["content"])) else {
            continue;
        };
        let Some(new) = latest(old) else {
            continue;
        };
        let mut events = Vec::new();
        if !room_state.iter().any(|event| {
            event["type"] == event_type && event["state_key"] == new && has_via(&event["content"])
        }) {
            events.push((event_type, new.to_string(), event["content"].clone()));
        }
        events.push((event_type, old.to_string(), json!({})));
        let description = if event_type == "m.space.parent" {
            format!("Replace {old} with {new} as a parent space of {room}")
        } else {
            format!("Replace {old} with {new} as a child of {room}")
        };
        changes.push(Change {
            description,
            upgraded_rooms: vec![old.to_string()],
            events,
        });
    }
    changes
}

/// The references to upgraded rooms that the user agreed to fix after upgrading the rooms in the
/// config, decided before upgrading any of them.
struct Fixes {
    /// Replacement room IDs by the ID of the room they replace, for the rooms that were upgraded
    /// before this run.
    replacements: HashMap<String, String>,
    /// The upgraded rooms each room we're in may stop referring to, by the ID of that room.
    approved: HashMap<String, HashSet<String>>,
}

/// Looks through the rooms we're in for references to upgraded rooms, or rooms in the config that
/// are about to be, and asks which of them to fix after upgrading the rooms in the config. Rooms in
/// the config and their replacements are fixed without asking. Also warns about aliases still
/// leading to upgraded rooms outside the config, and offers to lock them down if they aren't yet.
async fn plan_fixes(
    http_client: &reqwest::Client,
    config: &config::Config,
    self_user_id: &str,
    failures: &mut Failures,
) -> anyhow::Result<Fixes> {
    let homeserver_url = &config.homeserver_url;
    let mut room_states = Vec::new();
    for room in joined_rooms(http_client, homeserver_url).await? {
        match room_state(http_client, homeserver_url, &room).await {
            Ok(room_state) => room_states.push((room, room_state)),
            Err(err) => {
                warn!("Failed to get the state of {room}: {err:#}");
                failures.add("getting the state of rooms");
            }
        }
    }
    let mut replacements = HashMap::new();
    add_replacements(&mut replacements, &room_states);
    let configured: HashSet<&str> = config
        .rooms
        .iter()
        .flat_map(|room| {
            std::iter::once(room.as_str()).chain(replacement_chain(&replacements, room))
        })
        .collect();
    // The rooms in the config that aren't upgraded yet will be, to rooms we don't know yet.
    let mut planned_replacements = replacements.clone();
    for room in &config.rooms {
        planned_replacements
            .entry(room.clone())
            .or_insert_with(|| format!("the replacement of {room}"));
    }

    let admin_api = admin_api_available(http_client, homeserver_url, self_user_id).await?;
    let mut approved: HashMap<String, HashSet<String>> = HashMap::new();
    for (room, room_state) in &room_states {
        let power = Power::new(room_state)?;
        let mut level = power.user(self_user_id)?;
        if let Some(replacement) = replacements.get(room) {
            if !configured.contains(room.as_str()) {
                match aliases_left_behind(http_client, homeserver_url, room, &power).await {
                    Ok(left_behind) if left_behind.is_empty() => {}
                    Ok(left_behind) => warn!(
                        "{room} was upgraded to {replacement}, but {} still point to it",
                        left_behind.join(", ")
                    ),
                    Err(err) => {
                        warn!("Failed to check whether aliases still point to {room}: {err:#}");
                        failures.add("checking for aliases of upgraded rooms");
                    }
                }
            }
            // Rooms in the config are locked down when upgrading them, unless we were told not to.
            let needed = power.lock_down()?;
            if configured.contains(room.as_str())
                || needed == int!(0)
                || !confirm(format!(
                    "{room} was upgraded to {replacement}, but isn't locked down yet. Lock it down?"
                ))?
            {
                continue;
            }
            if admin_api && level < needed {
                level = offer_make_room_admin(
                    http_client,
                    homeserver_url,
                    self_user_id,
                    room,
                    &power,
                    level,
                    &needed.to_string(),
                )
                .await?;
            }
            if level < needed {
                warn!("Not locking down {room}, as we have power level {level} but need {needed}");
            } else if dry_run() {
                info!("Would lock down {room}");
            } else if let Err(err) = restrict_old_room(http_client, homeserver_url, room).await {
                warn!("Failed to lock down {room}: {err:#}");
                failures.add("locking down rooms");
            }
            continue;
        }
        for change in outdated_references(room, room_state, &planned_replacements, |_| true) {
            let description = &change.description;
            if !configured.contains(room.as_str()) && !confirm(format!("{description}?"))? {
                continue;
            }
            let mut needed = int!(0);
            for (event_type, _, _) in &change.events {
                needed = needed.max(power.state_event(event_type)?);
            }
            if admin_api && level < needed {
                level = offer_make_room_admin(
                    http_client,
                    homeserver_url,
                    self_user_id,
                    room,
                    &power,
                    level,
                    &needed.to_string(),
                )
                .await?;
            }
            if level < needed {
                warn!("Skipped: {description}, as we have power level {level} but need {needed}");
                continue;
            }
            if dry_run() {
                info!("Would do: {description}");
            }
            approved
                .entry(room.clone())
                .or_default()
                .extend(change.upgraded_rooms);
        }
    }
    Ok(Fixes {
        replacements,
        approved,
    })
}

/// Makes the rooms we're in refer to the current replacements of upgraded rooms where `fixes`
/// says to, and makes the replacements of the rooms in the config do so too.
async fn apply_fixes(
    http_client: &reqwest::Client,
    config: &config::Config,
    self_user_id: &str,
    fixes: Fixes,
    failures: &mut Failures,
) -> anyhow::Result<()> {
    let homeserver_url = &config.homeserver_url;
    let Fixes {
        mut replacements,
        mut approved,
    } = fixes;
    let mut rooms: HashSet<String> = approved.keys().cloned().collect();
    for room in &config.rooms {
        let tombstone =
            match get_state(http_client, homeserver_url, room, "m.room.tombstone", "").await {
                Ok(tombstone) => tombstone,
                Err(err) => {
                    warn!("Failed to find out what {room} was upgraded to: {err:#}");
                    failures.add("updating references to upgraded rooms");
                    continue;
                }
            };
        if let Some(new_room_id) = tombstone
            .as_ref()
            .and_then(|tombstone| tombstone["replacement_room"].as_str())
        {
            replacements.insert(room.clone(), new_room_id.to_string());
            // What the new room refers to comes from the old room, which is in the config.
            rooms.insert(new_room_id.to_string());
            approved.insert(
                new_room_id.to_string(),
                replacements.keys().cloned().collect(),
            );
        }
    }

    for room in rooms {
        // Upgraded rooms, like the ones in the config now, aren't used anymore.
        if replacements.contains_key(&room) {
            continue;
        }
        let room_state = match room_state(http_client, homeserver_url, &room).await {
            Ok(room_state) => room_state,
            Err(err) => {
                warn!("Failed to get the state of {room}: {err:#}");
                failures.add("updating references to upgraded rooms");
                continue;
            }
        };
        let power = Power::new(&room_state)?;
        let level = power.user(self_user_id)?;
        let fix = |upgraded_room: &str| {
            approved
                .get(&room)
                .is_some_and(|upgraded_rooms| upgraded_rooms.contains(upgraded_room))
        };
        for change in outdated_references(&room, &room_state, &replacements, fix) {
            let description = &change.description;
            let mut needed = int!(0);
            for (event_type, _, _) in &change.events {
                needed = needed.max(power.state_event(event_type)?);
            }
            if level < needed {
                warn!("Skipped: {description}, as we have power level {level} but need {needed}");
                continue;
            }
            let result = async {
                for (event_type, state_key, content) in &change.events {
                    put_state(
                        http_client,
                        homeserver_url,
                        &room,
                        event_type,
                        state_key,
                        content,
                    )
                    .await?;
                }
                anyhow::Ok(())
            }
            .await;
            match result {
                Ok(()) => info!("Done: {description}"),
                Err(err) => {
                    warn!("Failed: {description}: {err:#}");
                    failures.add("updating references to upgraded rooms");
                }
            }
        }
    }
    Ok(())
}

/// Describes what still leads people to `room` through aliases or the room directory, which only
/// upgrading the rooms in the config moves to their replacements.
async fn aliases_left_behind(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    power: &Power<'_>,
) -> anyhow::Result<Vec<&'static str>> {
    let mut left_behind = Vec::new();
    if power.has_canonical_alias() {
        left_behind.push("its canonical alias");
    }
    if !local_aliases(http_client, homeserver_url, room)
        .await?
        .is_empty()
    {
        left_behind.push("its local aliases");
    }
    if is_published(http_client, homeserver_url, room).await? {
        left_behind.push("its room directory listing");
    }
    Ok(left_behind)
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
