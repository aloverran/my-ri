use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::tracing_broadcast::{LogBuffer, LogEntry};

/// App-wide events broadcast to all connected clients (not per-session).
/// Used for global UI concerns like desktop notifications.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum GlobalEvent {
    /// An agent loop finished and the session is now idle, awaiting input.
    #[serde(rename = "session_done")]
    SessionDone {
        session_id: String,
        name: String,
        /// Short preview of the final assistant message text, if any.
        preview: Option<String>,
        /// Non-null for sub-agent sessions spawned by runAgent.
        parent: Option<String>,
    },
}

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
    /// Broadcast channel for live tracing log entries.
    pub log_tx: broadcast::Sender<LogEntry>,
    /// Ring buffer of all log entries since boot (capped). Snapshotted
    /// when a new SSE client connects so it sees full history.
    pub log_buffer: Arc<LogBuffer>,
    /// Global event broadcast for app-wide notifications (session done, etc).
    pub global_tx: broadcast::Sender<GlobalEvent>,
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

/// Default name for newly created sessions, before title generation runs.
pub const DEFAULT_SESSION_NAME: &str = "New session";

/// Per-session state. Behind Arc<Mutex<>> in the sessions map.
pub struct SessionState {
    pub store: ri::Store,
    pub message_ids: Vec<ri::MessageId>,
    pub cwd: PathBuf,
    pub name: String,
    pub ts: String,
    /// File-stem ID of this session (e.g. "2026-02-24_201128_my-task").
    pub file_id: ri::SessionId,
    /// File-stem ID of the parent session, if this session was spawned by another.
    pub parent: Option<ri::SessionId>,
    /// Broadcast channel for SSE clients to subscribe to agent events.
    pub events_tx: broadcast::Sender<AgentEvent>,
    /// Active agent run handle. None when idle.
    pub current_run: Option<RunHandle>,
    /// Monotonic counter for background title generation. Each new title
    /// task captures the current value; when it finishes, it only applies
    /// the result if the counter hasn't advanced (preventing stale overwrites).
    pub title_gen_seq: u64,
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
