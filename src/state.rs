use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub struct State {
    #[serde(skip)]
    path: PathBuf,
    /// New room IDs by the ID of the room they replace, recorded before the old room is
    /// tombstoned so that an interrupted upgrade resumes with the room it already created.
    pub replacement_rooms: HashMap<String, String>,
    /// Upgrade notice event IDs by the ID of the room they were sent in, recorded until the new
    /// room is created so that retrying doesn't send another notice.
    #[serde(default)]
    pub upgrade_notices: HashMap<String, String>,
    /// Old room IDs by the alias that pointed to them, recorded before the alias is deleted so
    /// that an interrupted upgrade still points it to the new room.
    #[serde(default)]
    pub moving_aliases: HashMap<String, String>,
}

impl State {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let mut state: Self = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(err) if err.kind() == ErrorKind::NotFound => Self::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        state.path = path;
        Ok(state)
    }

    /// Writes the state to a temporary file and renames it into place, so that a crash while
    /// saving never leaves a truncated state file behind.
    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let tmp_path = self.path.with_extension("json.tmp");
        fs::write(&tmp_path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}
