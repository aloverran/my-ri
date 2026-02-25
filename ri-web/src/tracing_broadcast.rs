//! A tracing Layer that sends structured log entries to a broadcast channel
//! and accumulates them in a ring buffer for replay on new SSE connections.
//!
//! The BroadcastLayer does two things on each tracing event:
//!   1. Pushes a LogEntry into the shared LogBuffer (sync Mutex, brief hold)
//!   2. Sends it to the broadcast channel for live SSE subscribers
//!
//! When a new SSE client connects, it snapshots the buffer first (full
//! history since boot, capped), then switches to the live broadcast stream.

use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// A single log entry, sent to the frontend as JSON via SSE.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// Log level as a precise string enum for frontend filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<&tracing::Level> for LogLevel {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        }
    }
}

/// Capped ring buffer of log entries. Shared between the tracing layer
/// (which pushes) and the SSE handler (which snapshots on connect).
/// Uses std::sync::Mutex because the tracing layer runs synchronously.
pub struct LogBuffer {
    inner: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(65536))),
            capacity,
        }
    }

    fn push(&self, entry: LogEntry) {
        let mut buf = self.inner.lock().unwrap();
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Clone out all buffered entries. Called once per SSE connection.
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }
}

/// Tracing layer that formats events, buffers them, and broadcasts them.
pub struct BroadcastLayer {
    tx: broadcast::Sender<LogEntry>,
    buffer: std::sync::Arc<LogBuffer>,
}

impl BroadcastLayer {
    pub fn new(tx: broadcast::Sender<LogEntry>, buffer: std::sync::Arc<LogBuffer>) -> Self {
        Self { tx, buffer }
    }
}

impl<S> Layer<S> for BroadcastLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let now = chrono::Local::now();
        let ts = now.format("%H:%M:%S%.3f").to_string();

        let level = LogLevel::from(event.metadata().level());
        let target = event.metadata().target().to_string();

        let mut message = String::new();
        let mut visitor = FieldVisitor(&mut message);
        event.record(&mut visitor);

        let entry = LogEntry {
            ts,
            level,
            target,
            message,
        };
        self.buffer.push(entry.clone());
        let _ = self.tx.send(entry);
    }
}

/// Visitor that extracts the message field and appends other fields.
struct FieldVisitor<'a>(&'a mut String);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            write!(self.0, " {}={}", field.name(), value).unwrap();
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            write!(self.0, "{:?}", value).unwrap();
        } else {
            write!(self.0, " {}={:?}", field.name(), value).unwrap();
        }
    }
}
