use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use reqwest::{header, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

mod cli;
mod config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let config_file = File::open(cli.config)?;
    let config: config::Config = serde_yaml::from_reader(config_file)?;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {}", config.access_token))?,
    );

    let http_client = reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .default_headers(headers)
        .build()?;

    let self_user_id_res = dbg!(
        dbg!(
            send(http_client.get(format!(
                "{}/_matrix/client/v3/account/whoami",
                config.homeserver_url
            )))
            .await
        )?
        .json::<Value>()
        .await
    )?;
    let self_user_id = dbg!(self_user_id_res["user_id"].as_str().unwrap().to_string());

    for room in config.rooms {
        let tombstone = send(http_client.get(format!(
            "{}/_matrix/client/v3/rooms/{room}/state/m.room.tombstone/",
            config.homeserver_url
        )))
        .await?;
        let new_room_id = if tombstone.status() == StatusCode::OK {
            println!("Room upgraded already, only transferring membership state");
            let tombstone_content: serde_json::Value = dbg!(tombstone.json().await?);
            Some(
                tombstone_content
                    .get("replacement_room")
                    .context("no replacement_room, should be part of tombstone event")?
                    .as_str()
                    .context("no string wtf")?
                    .to_string(),
            )
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
            dbg!(member);
            match member["content"]["membership"].as_str().unwrap() {
                "join" => joined_members.push((
                    member["state_key"].as_str().unwrap().to_string(),
                    member["content"]["reason"]
                        .as_str()
                        .map(|str| str.to_string()),
                )),
                "invite" => joined_members.push((
                    member["state_key"].as_str().unwrap().to_string(),
                    member["content"]["reason"]
                        .as_str()
                        .map(|str| str.to_string()),
                )),
                "ban" => banned_members.push((
                    member["state_key"].as_str().unwrap().to_string(),
                    member["content"]["reason"]
                        .as_str()
                        .map(|str| str.to_string()),
                )),
                _ => {}
            }
        }

        dbg!(&banned_members);
        dbg!(&joined_members);

        let new_room_id = if new_room_id.is_none() {
            let mut state: HashMap<String, Value> = HashMap::new();
            for event_type in &config.state_events_to_transfer {
                let res = send(http_client.get(format!(
                    "{}/_matrix/client/v3/rooms/{room}/state/{event_type}/",
                    config.homeserver_url
                )))
                .await?;
                if res.status() != StatusCode::OK {
                    continue;
                }
                let mut val: Value = res.json().await?;
                if event_type == "m.room.power_levels" {
                    let map = val.as_object_mut().context("PL state is not an object")?;
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
                        println!(
                            "Overrode power level for user {user_id} in room {room} to be {pl}"
                        )
                    }

                    if config.target_room_version >= 12 {
                        users.remove(&self_user_id);
                    }
                }
                state.insert(event_type.to_string(), val);
            }
            println!("New state for {room}: {state:#?}");

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
            let last_event_id = dbg!(dbg!(res).json::<Value>().await?)["event_id"]
                .as_str()
                .context("event_id is not a string")?
                .to_string();

            println!("Last event ID: {last_event_id}");

            let power_level_content_override = dbg!(state.remove("m.room.power_levels").unwrap());
            let initial_state: Vec<_> = dbg!(state
                .into_iter()
                .filter(|(event_type, _)| event_type != "m.room.power_levels")
                .map(|(event_type, content)| {
                    json!({
                        "content": content,
                        "type": event_type,
                    })
                })
                .collect());

            let target_room_version = format!("{}", config.target_room_version);

            let new_room_res = dbg!(
                send(
                    http_client
                        .post(format!(
                            "{}/_matrix/client/v3/createRoom",
                            config.homeserver_url
                        ))
                        .json(dbg!(&json!({
                            "creation_content": {
                                "predecessor": {
                                    "event_id": last_event_id,
                                    "room_id": room,
                                },
                            },
                            "room_version": target_room_version,
                            "power_level_content_override": power_level_content_override,
                            "initial_state": initial_state,
                        })))
                )
                .await
            )?;
            let new_room_body = dbg!(new_room_res.json::<Value>().await?);
            let new_room_id = new_room_body["room_id"]
                .as_str()
                .context("room id is not a string")?;

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
            new_room_id.to_string()
        } else {
            new_room_id
                .context("we're in the else clause of a if is_none, it should be here really")?
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
        dbg!(&new_members);

        for (user_id, reason) in banned_members.iter() {
            if new_members.contains(user_id.as_str()) {
                continue;
            }
            dbg!(
                dbg!(
                    send(
                        http_client
                            .post(format!(
                                "{}/_matrix/client/v3/rooms/{new_room_id}/ban",
                                config.homeserver_url
                            ))
                            .json(&json!({
                                "reason": reason,
                                "user_id": user_id,
                            }))
                    )
                    .await?
                )
                .text()
                .await?
            );
        }

        for (user_id, reason) in joined_members.iter() {
            if dbg!(config.drop_members.contains(dbg!(user_id))) {
                continue;
            } else if &self_user_id == user_id {
                continue;
            } else if new_members.contains(user_id.as_str()) {
                continue;
            }
            dbg!(
                dbg!(
                    send(
                        http_client
                            .post(format!(
                                "{}/_matrix/client/v3/rooms/{new_room_id}/invite",
                                config.homeserver_url
                            ))
                            .json(&json!({
                                "reason": reason,
                                "user_id": user_id,
                            }))
                    )
                    .await?
                )
                .text()
                .await?
            );
        }
    }
    Ok(())
}

/// Sends a request, retrying while the server rate limits us. Waits as long as the server asks
/// via `Retry-After` or `retry_after_ms`, otherwise backs off exponentially up to `MAX_BACKOFF`.
async fn send(request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let res = request
            .try_clone()
            .context("request should be cloneable")?
            .send()
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
        println!("Rate limited, retrying in {wait:?}");
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
