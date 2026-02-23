use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;

/// Top-level server state, shared across all handlers via Arc.
pub struct AppState {
    /// All tools available to the primary agent (base + meta).
    pub tools: Vec<Arc<dyn ri::Tool>>,
    /// Base tools only (bash, read, write, edit) -- given to sub-agents
    /// spawned by the runAgent meta-tool.
    pub base_tools: Vec<Arc<dyn ri::Tool>>,
    /// Global defaults (from CLI flags / settings.json). Used when a session
    /// has no history to pull from.
    pub default_model: String,
    pub default_thinking: ri::ThinkingLevel,
    pub sessions_dir: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Mutex<SessionState>>>>,
}

/// Per-session state. Behind Arc<Mutex<>> in the sessions map.
pub struct SessionState {
    pub store: ri::SessionStore,
    pub message_ids: Vec<String>,
    pub cwd: PathBuf,
    pub name: String,
    pub ts: String,
    /// Broadcast channel for SSE clients to subscribe to agent events.
    pub events_tx: broadcast::Sender<AgentEvent>,
    /// Active agent run handle. None when idle.
    pub current_run: Option<RunHandle>,
}

/// Handle to a running agent loop. One per run, not reused.
/// The JoinHandle is held to keep the task alive, not read directly.
#[allow(dead_code)]
pub struct RunHandle {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
}

impl SessionState {
    pub fn is_running(&self) -> bool {
        self.current_run.is_some()
    }

    pub fn status(&self) -> &'static str {
        if self.is_running() { "running" } else { "idle" }
    }
}
