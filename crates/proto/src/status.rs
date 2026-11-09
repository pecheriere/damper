//! The daemon's status snapshot.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::{InputsSummary, LimitReason};
use crate::msg::SessionId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub limit: u32,
    pub reason: LimitReason,
    pub cores: u32,
    pub memory: u64,
    pub inputs: InputsSummary,
    pub holding: bool,
    pub sessions: Vec<SessionStatus>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session: SessionId,
    pub cwd: PathBuf,
    pub subcommand: String,
    pub state: SessionState,
    pub slots: u32,
    pub tokens_outstanding: u32,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Held { waited_secs: u64 },
    Compiling,
    Idle,
}
