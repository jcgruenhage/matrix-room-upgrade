use std::collections::HashSet;
use std::fs::File;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use log::{debug, error, info, warn, LevelFilter};
use reqwest::{header, StatusCode};
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

    let mut failed_rooms = Vec::new();
    for room in &config.rooms {
        if let Err(err) = upgrade_room(&http_client, &config, &mut state, &self_user_id, room).await
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
    )
    .await?
    .context("room has no power levels")?;
    let map = power_levels
        .as_object_mut()
        .context("PL state is not an object")?;
    let users_default = match map.get("users_default") {
        Some(num) => num
            .as_number()
            .context("PL state key users_default is not a number")?
            .as_u64()
            .context("PL state key users_default is not a u64")?,
        None => 0,
    };
    let users = map
        .get_mut("users")
        .context("PL state does not contain users key")?
        .as_object_mut()
        .context("PL state key users is not an object")?;

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
        // Power levels are passed separately as power_level_content_override.
        if event_type == "m.room.power_levels" {
            continue;
        }
        if let Some(content) =
            get_state(http_client, &config.homeserver_url, room, event_type).await?
        {
            initial_state.push(json!({
                "content": content,
                "type": event_type,
            }));
        }
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

async fn upgrade_room(
    http_client: &reqwest::Client,
    config: &config::Config,
    state: &mut state::State,
    self_user_id: &str,
    room: &str,
) -> anyhow::Result<()> {
    info!("Upgrading {room}");
    let tombstone = get_state(
        http_client,
        &config.homeserver_url,
        room,
        "m.room.tombstone",
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

        send(
            http_client
                .put(format!(
                    "{}/_matrix/client/v3/rooms/{room}/state/m.room.tombstone/",
                    config.homeserver_url
                ))
                .json(&json!({
                    "body": "This room has been replaced",
                    "replacement_room": new_room_id,
                })),
        )
        .await?
        .json::<Value>()
        .await?;
        info!("Tombstoned {room}");
        new_room_id
    };

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

    let mut failures = 0;
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
        "{failures} bans/invites in {new_room_id} failed, re-run to retry them"
    );
    Ok(())
}

/// Fetches the content of a state event with an empty state key, or `None` if the room has no
/// such state event.
async fn get_state(
    http_client: &reqwest::Client,
    homeserver_url: &str,
    room: &str,
    event_type: &str,
) -> anyhow::Result<Option<Value>> {
    let res = send_retrying(http_client.get(format!(
        "{homeserver_url}/_matrix/client/v3/rooms/{room}/state/{event_type}/"
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
