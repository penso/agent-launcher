use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::RunState;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub run_id: String,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub payload: RunEvent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    StateChanged {
        state: RunState,
        message: Option<String>,
    },
    Output {
        stream: OutputStream,
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolStarted {
        name: String,
    },
    ToolFinished {
        name: String,
        success: bool,
    },
    InputRequested {
        prompt: String,
    },
    PermissionRequested {
        id: String,
        description: String,
    },
    Completed {
        success: bool,
        message: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Pty,
}
