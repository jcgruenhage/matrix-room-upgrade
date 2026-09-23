use std::collections::HashMap;

use js_int::Int;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct Config {
    pub homeserver_url: String,
    pub access_token: String,
    pub target_room_version: usize,
    pub rooms: Vec<String>,
    pub pl_overrides: HashMap<String, Int>,
    pub state_events_to_transfer: Vec<String>,
    pub drop_members: Vec<String>,
    /// Whether to ban users banned in the old room in the new one too.
    #[serde(default = "transfer_bans_default")]
    pub transfer_bans: bool,
    /// Users whose bans aren't transferred, like moderation bots that ban users in the new room
    /// themselves.
    #[serde(default)]
    pub skip_bans_by: Vec<String>,
}

fn transfer_bans_default() -> bool {
    true
}

impl Config {
    /// Whether the ban that `ban`, a member event in the old room, stands for should be
    /// transferred to the new room.
    pub fn transfers_ban(&self, ban: &Value) -> bool {
        self.transfer_bans
            && !ban["sender"]
                .as_str()
                .is_some_and(|sender| self.skip_bans_by.iter().any(|user| user == sender))
    }
}
