//! Supervisor mode for `--watch`: watches source files, rebuilds on change,
//! manages a child server process with graceful restart.
//!
//! The supervisor is a thin loop. It does not serve HTTP, does not proxy,
//! and does not know about agents or sessions. Three responsibilities:
//!
//!   1. Watch source files for changes (via notify + debounce).
//!   2. Run `cargo build` and check if the output binary changed.
//!   3. Manage the child process lifecycle (spawn, signal, wait, respawn).
//!
//! IPC is over the child's stdin pipe: `"update\n"` signals a new binary
//! is ready. EOF (pipe closed) tells the child the supervisor died.
//!
//! The rename-before-build pattern frees the cargo output path on all
//! platforms (including Windows, where running .exe files are locked)
//! and doubles as the rollback mechanism on build failure.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// Whether this binary was built in release mode.
const IS_RELEASE: bool = !cfg!(debug_assertions);

/// Entry point for supervisor mode. Never returns.
pub async fn run_supervisor() -> ! {
    let (workspace_root, watch_paths) = discover_watch_paths();
    let exe_path = cargo_output_path(&workspace_root);

    tracing::info!(
        "supervisor starting, watching {} paths",
        watch_paths.len()
    );
    for p in &watch_paths {
        tracing::debug!("watching [{}]", p.display());
    }

    // Initial build to ensure the child binary is fresh.
    if !build(&workspace_root, &exe_path).await {
        tracing::error!("initial build failed, cannot start");
        std::process::exit(1);
    }

    let child_args = collect_child_args();
    let mut child = spawn_child(&exe_path, &child_args);

    // Take stdin out of the Child so that `child.wait()` doesn't close
    // the pipe (tokio drops stdin before waiting to avoid deadlocks).
    // We hold the write end separately for the lifetime of the child.
    let mut child_stdin = child.stdin.take().expect("child stdin was piped");

    // Monitor child exit in a separate task. This avoids the issue where
    // dropping and re-creating the `child.wait()` future in a select loop
    // can lose the exit notification (tokio's background wait consumes it).
    let (child_exit_tx, mut child_exit_rx) =
        tokio::sync::mpsc::channel::<std::process::ExitStatus>(1);
    tokio::spawn({
        async move {
            match child.wait().await {
                Ok(status) => {
                    let _ = child_exit_tx.send(status).await;
                }
                Err(e) => {
                    tracing::error!("waiting for child failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
    });

    // Debounced file watcher -> tokio channel bridge.
    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::unbounded_channel();
    let _debouncer = setup_watcher(&watch_paths, watch_tx);

    // -- Main loop --
    //
    // States are implicit rather than an enum: the select branches and
    // local flags (rebuild_pending, update_signaled) capture them cleanly
    // without the boilerplate of a state machine type.

    let mut rebuild_pending = false;
    let mut update_signaled = false;

    loop {
        tokio::select! {
            Some(status) = child_exit_rx.recv() => {
                match status.code() {
                    Some(42) => {
                        tracing::info!("child requested restart (exit 42), spawning new process");
                        let mut new_child = spawn_child(&exe_path, &child_args);
                        child_stdin = new_child.stdin.take().expect("child stdin was piped");
                        update_signaled = false;

                        // Spawn a new child monitor task.
                        let (tx, rx) = tokio::sync::mpsc::channel(1);
                        child_exit_rx = rx;
                        tokio::spawn(async move {
                            match new_child.wait().await {
                                Ok(s) => { let _ = tx.send(s).await; }
                                Err(e) => {
                                    tracing::error!("waiting for child failed: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        });

                        if rebuild_pending {
                            rebuild_pending = false;
                            tracing::info!("rebuild pending, will trigger on next file change");
                        }
                    }
                    other => {
                        let code = other.unwrap_or(1);
                        tracing::info!("child exited with code [{}], supervisor exiting", code);
                        std::process::exit(code);
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                // Child got SIGINT too (same process group). Just wait for it.
                tracing::info!("supervisor received ctrl-c, waiting for child to exit");
                drop(child_stdin); // close pipe so child sees EOF
                if let Some(status) = child_exit_rx.recv().await {
                    std::process::exit(status.code().unwrap_or(1));
                }
                std::process::exit(1);
            }

            Some(_) = watch_rx.recv() => {
                tracing::info!("source change detected, rebuilding");

                if !build(&workspace_root, &exe_path).await {
                    // Build failed. Errors are visible in the terminal
                    // (cargo stderr is inherited). Resume watching.
                    continue;
                }

                if update_signaled {
                    // Already waiting for the child to restart from a
                    // previous build. Queue another build after restart.
                    rebuild_pending = true;
                    tracing::info!("update already pending, will rebuild after restart");
                    continue;
                }

                // Signal the child that a new binary is ready.
                let _ = child_stdin.write_all(b"update\n").await;
                let _ = child_stdin.flush().await;
                update_signaled = true;
                tracing::info!("signaled child: update available");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Watch path discovery
// ---------------------------------------------------------------------------

/// Run `cargo metadata` to find the workspace root and all local crate
/// source directories. Returns (workspace_root, watch_paths).
fn discover_watch_paths() -> (PathBuf, Vec<PathBuf>) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("Cargo.toml");

    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&manifest)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to run cargo metadata");

    if !output.status.success() {
        eprintln!("cargo metadata failed");
        std::process::exit(1);
    }

    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("failed to parse cargo metadata JSON");

    let workspace_root = PathBuf::from(
        meta["workspace_root"]
            .as_str()
            .expect("missing workspace_root in cargo metadata"),
    );

    let mut paths: Vec<PathBuf> = Vec::new();

    if let Some(packages) = meta["packages"].as_array() {
        for pkg in packages {
            // Local packages have source: null.
            if !pkg["source"].is_null() {
                continue;
            }

            if let Some(manifest_path) = pkg["manifest_path"].as_str() {
                let manifest = PathBuf::from(manifest_path);
                if let Some(pkg_dir) = manifest.parent() {
                    let src = pkg_dir.join("src");
                    if src.is_dir() {
                        paths.push(src);
                    }
                    // Watch the crate's own Cargo.toml.
                    paths.push(manifest.clone());
                }
            }
        }
    }

    // Workspace-level files.
    let lock = workspace_root.join("Cargo.lock");
    if lock.exists() {
        paths.push(lock);
    }

    (workspace_root, paths)
}

// ---------------------------------------------------------------------------
// File watcher
// ---------------------------------------------------------------------------

/// Create a debounced file watcher that sends events through a tokio channel.
fn setup_watcher(
    paths: &[PathBuf],
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> notify_debouncer_mini::Debouncer<notify_debouncer_mini::notify::RecommendedWatcher> {
    let mut debouncer = new_debouncer(Duration::from_millis(300), move |res: notify_debouncer_mini::DebounceEventResult| match res {
        Ok(events) => {
            // Only trigger on Rust-relevant files.
            let dominated = events.iter().any(|e| is_relevant(&e.path));
            if dominated {
                let _ = tx.send(());
            }
        }
        Err(errs) => {
            tracing::warn!("file watch error: {}", errs);
        }
    })
    .expect("failed to create file watcher");

    for path in paths {
        let mode = if path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        if let Err(e) = debouncer.watcher().watch(path, mode) {
            tracing::warn!("failed to watch [{}]: {}", path.display(), e);
        }
    }

    debouncer
}

/// Only trigger rebuilds for Rust source and config files.
fn is_relevant(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs" | "toml" | "lock") => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Rename the cargo output to `.prev`, run `cargo build`, and check
/// whether a new binary was produced. On failure, renames `.prev` back.
/// Returns true if a new binary is ready.
async fn build(workspace_root: &Path, exe_path: &Path) -> bool {
    let prev_path = exe_path.with_extension(prev_extension());

    // Rename current binary out of the way (frees the path on Windows,
    // creates the rollback backup on all platforms).
    if exe_path.exists() {
        if let Err(e) = std::fs::rename(exe_path, &prev_path) {
            tracing::warn!(
                "failed to rename [{}] to [{}]: {}",
                exe_path.display(),
                prev_path.display(),
                e
            );
            // On macOS/Linux this isn't critical (cargo can overwrite
            // running binaries). Continue with the build.
        }
    }

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "-p", "ri-web"]);
    if IS_RELEASE {
        cmd.arg("--release");
    }
    cmd.current_dir(workspace_root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = match cmd.status().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to spawn cargo: {}", e);
            rollback_rename(exe_path, &prev_path);
            return false;
        }
    };

    if !status.success() {
        tracing::warn!("build failed (exit {})", status.code().unwrap_or(-1));
        rollback_rename(exe_path, &prev_path);
        return false;
    }

    // Check if cargo actually produced a new binary.
    if exe_path.exists() {
        tracing::info!("build succeeded, new binary ready");
        true
    } else {
        // Cargo decided nothing needed recompiling (shouldn't happen
        // since a file change triggered this, but handle defensively).
        tracing::info!("build succeeded but binary unchanged");
        rollback_rename(exe_path, &prev_path);
        false
    }
}

/// Move `.prev` back to the original path if the build failed.
fn rollback_rename(exe_path: &Path, prev_path: &Path) {
    if prev_path.exists() && !exe_path.exists() {
        let _ = std::fs::rename(prev_path, exe_path);
    }
}

/// The `.prev` extension, accounting for Windows `.exe` suffix.
fn prev_extension() -> String {
    let suffix = std::env::consts::EXE_SUFFIX;
    if suffix.is_empty() {
        "prev".to_string()
    } else {
        // e.g. "prev.exe"
        format!("prev{}", suffix)
    }
}

// ---------------------------------------------------------------------------
// Child process
// ---------------------------------------------------------------------------

/// Spawn the server as a child process with stdin piped for IPC.
fn spawn_child(exe_path: &Path, args: &[String]) -> Child {
    tracing::info!("spawning child [{}]", exe_path.display());

    Command::new(exe_path)
        .args(args)
        .env("RI_WEB_SUPERVISED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            tracing::error!("failed to spawn child: {}", e);
            std::process::exit(1);
        })
}

/// Collect the CLI args to pass to the child, stripping `--watch`.
fn collect_child_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| a != "--watch")
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive the cargo output binary path from the workspace root and
/// the current build profile.
fn cargo_output_path(workspace_root: &Path) -> PathBuf {
    let profile_dir = if IS_RELEASE { "release" } else { "debug" };
    let exe_name = format!("ri-web{}", std::env::consts::EXE_SUFFIX);
    workspace_root
        .join("target")
        .join(profile_dir)
        .join(exe_name)
}
