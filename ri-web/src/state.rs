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
    /// In-progress OAuth login flows, keyed by provider id.
    /// Holds the provider instance (which stores the PKCE verifier internally)
    /// and tracks whether the flow has completed.
    pub logins: RwLock<HashMap<String, LoginInProgress>>,
}

/// An OAuth login flow in progress. The provider instance must be kept alive
/// between begin_login() and complete_login() because it holds the PKCE
/// verifier in its internal state.
pub struct LoginInProgress {
    pub provider: Mutex<Box<dyn ri::LlmProvider>>,
    pub status: Arc<Mutex<LoginStatus>>,
}

#[derive(Debug, Clone)]
pub enum LoginStatus {
    /// PasteCode flow: waiting for user to paste the auth code.
    AwaitingCode,
    /// LocalCallback flow: temp server running, waiting for browser redirect.
    AwaitingCallback,
    /// Login completed successfully.
    Complete,
    /// Login failed with an error message.
    Failed(String),
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
