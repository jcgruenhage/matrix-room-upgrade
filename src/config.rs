use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub homeserver_url: String,
    pub access_token: String,
    pub target_room_version: usize,
    pub rooms: Vec<String>,
    pub pl_overrides: HashMap<String, u64>,
    pub state_events_to_transfer: Vec<String>,
    pub drop_members: Vec<String>,
}
