# ri-web: Self-Reloading via `--watch`

## What This Is

A `--watch` flag for ri-web that monitors its own source files, rebuilds on
change, waits for active agent runs to finish, and replaces itself by spawning
a new process. One binary, no external tooling, cross-platform.

```
ri-web --port 3001 --host 0.0.0.0 --watch
```


## Why

ri-web is a self-modifying system. An AI agent running inside the server can
edit the server's own source code. When that happens, we want the new code
running -- but safely. A broken build must never take the server down, and
active agent runs (long-running LLM API loops streaming via SSE) must not be
killed mid-stream.

Today, reloading requires manually stopping and restarting the process. This is
tedious during development and impossible when the agent edits its own code
(the agent can't restart the server it's running inside).


## Architecture

The entire feature lives inside ri-web as a background tokio task, gated behind
`--watch`. No external watcher process, no wrapper script, no separate binary.

### The four phases

```
[WATCH]              [RENAME]              [BUILD]              [DRAIN + REPLACE]
  |                    |                     |                     |
  | .rs file changed   | mv binary -> .prev  | cargo build         | wait for active runs = 0
  |------------------->| (frees output path  | exit code 0?        | trigger graceful shutdown
  |                    |  on all platforms)  |-> no: mv .prev back | spawn new process (same args)
  |                    |                     |-> yes: reload!      | exit old process
```

### What watches what

The `notify` crate provides cross-platform file watching (kqueue on macOS,
inotify on Linux, ReadDirectoryChanges on Windows). We watch:

- `ri-web/src/` -- the server's own code
- `ri-core/crates/` -- the three path dependencies (ri, ri-ai, ri-tools)
- `Cargo.toml` / `Cargo.lock` at the workspace root

Only `.rs` and `.toml` files trigger a rebuild. Everything else is ignored.
Events are debounced (~500ms) so that rapid saves or multi-file writes from an
agent settle before we build.


### The rename-before-build pattern

On Windows, a running .exe is locked against overwriting and deletion. But it
CAN be renamed. This is because renaming only modifies the filesystem directory
entry -- the memory-mapped file data and the process's open handle are
unaffected.

Before every build, we rename the current binary to `<path>.prev`:
```
target/debug/ri-web  ->  target/debug/ri-web.prev
```

This one operation solves two problems on every platform:
1. **Unblocks the linker on Windows.** The cargo output path is now free.
   (On Unix this is unnecessary but harmless -- the OS doesn't lock binaries.)
2. **Creates the rollback backup.** The `.prev` file is the last known-good
   binary. No separate copy step needed.

The running process is unaffected. It was loaded into memory at startup; the
file on disk is irrelevant to execution. The process continues serving from
memory regardless of what happens to the filename.

If the build fails, we rename `.prev` back to the original path. The filesystem
returns to its pre-build state. Nothing happened.


### The spawn-and-exit mechanism

When it's time to reload, the old process:

1. Triggers graceful shutdown on axum (stops accepting new connections)
2. Awaits the serve future returning (listener drops, port is freed)
3. Spawns the new binary as a new process with the same args
4. Exits

The new process starts, binds the port, and serves. The gap between the old
process releasing the port and the new one binding it is brief (under a second).
`SO_REUSEADDR` (set by tokio by default) ensures the port is immediately
reusable.

This is fully cross-platform. No `exec()`, no fd passing, no Unix-specific
APIs. `std::process::Command::new(path).args(args).spawn()` works everywhere.


### The drain

When a build succeeds, we don't restart immediately. Active agent runs are
in-flight tokio tasks making LLM API calls and streaming SSE to clients.
Killing them mid-stream loses the LLM response.

Instead:
1. Set `reload_requested: AtomicBool` in AppState
2. The agent run completion path (where `current_run` is cleared) checks this
   flag. If set, and no other sessions have active runs, initiate the reload.
3. If no runs are active at the moment the build succeeds, reload immediately.

We do NOT block new runs during drain. If a new run starts, it runs against the
old code and finishes naturally, then we reload. This avoids any "restart
pending, please wait" UX. The server is always fully functional until the
instant it spawns its replacement.


### The rollback

The safety model is layered:

1. **Build gate.** If `cargo build` fails, the `.prev` binary is renamed back
   and the server keeps running. Nothing changes. This is the primary safety
   net and handles the vast majority of bad code changes.

2. **The `.prev` binary.** On successful build, the old binary stays at the
   `.prev` path. If the new binary crashes on startup, the user can manually
   start `.prev`. This is a manual fallback, not automatic -- keeping it manual
   avoids the complexity of a supervisor/watchdog process.

Automatic rollback would require an outer supervisor process, which contradicts
the single-binary design. The build gate catches the vast majority of issues.
In Rust, if it compiles, it almost certainly starts.


## Implementation Plan

### 1. Add `--watch` CLI flag

```rust
#[arg(long)]
watch: bool,
```

### 2. Add reload state to AppState

```rust
pub reload_requested: AtomicBool,
pub shutdown: CancellationToken,
```

The `shutdown` token is used for axum's graceful shutdown. It's always present
(not --watch specific) since it's useful for clean SIGTERM handling too.

### 3. Implement the watch + build task

A single `tokio::spawn` that:
- Sets up a `notify::RecommendedWatcher` on the source directories
- Debounces events (500ms quiet period)
- On trigger:
  1. Rename binary to .prev
  2. Spawn `cargo build -p ri-web` via `tokio::process::Command`
  3. Stream cargo stdout/stderr to tracing at info level
  4. On build failure: rename .prev back, log the error, resume watching
  5. On build success: set `reload_requested`, check if we can reload now

### 4. Implement the drain + reload path

Two trigger points:
- **Immediate**: in the watch task, right after setting `reload_requested`,
  check if any runs are active. If none, reload now.
- **Deferred**: in the agent run completion path (where `current_run` is
  cleared), if `reload_requested` is set, check all sessions. If none have
  active runs, reload.

The reload itself:
```rust
fn reload(state: &AppState) {
    // The cargo output path -- this is the NEW binary.
    let binary = cargo_output_path();
    let args: Vec<String> = std::env::args().skip(1).collect();

    tracing::info!("reloading: spawning new process [{}]", binary.display());

    // Trigger graceful shutdown -- axum stops accepting, drains HTTP,
    // drops listener (freeing the port).
    state.shutdown.cancel();

    // Spawn the new binary.
    match std::process::Command::new(&binary).args(&args).spawn() {
        Ok(_) => {
            tracing::info!("new process spawned, exiting");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!("failed to spawn new process: {}", e);
            // Don't exit -- keep running the old code.
        }
    }
}

fn cargo_output_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is baked in at compile time.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let name = if cfg!(windows) { "ri-web.exe" } else { "ri-web" };
    manifest.join("..").join("target").join(profile).join(name)
}
```

### 5. Wire it up

- In main(): use `axum::serve(...).with_graceful_shutdown(shutdown.cancelled())`
- In main(): if `--watch`, spawn the watch task
- In agent.rs: after clearing `current_run`, call the drain-check


## Tricky Questions

### What if the agent starts a new run WHILE drain is pending?

It runs normally. We don't block it. The new run executes against old code,
finishes, and then the drain check triggers. This means the reload is delayed
further, but correctness is preserved. The alternative (blocking new runs)
creates UX issues and edge cases around "what if the user doesn't know a
reload is pending."

### What if two rapid file changes trigger two builds?

Debouncing handles the common case (save, auto-format, etc). For genuinely
separate rapid changes (agent writing two files in sequence), the first build
might start before the second file is written. Two possibilities:
- First build fails (incomplete code) -- .prev is restored, second change
  triggers another build that succeeds. Correct.
- First build succeeds (the files were independently valid) -- reload happens,
  then the second change triggers the watch again in the NEW process. Correct.

If a build is already in progress when a new change arrives: let the current
build finish, then immediately start another. Don't try to cancel the in-flight
build (killing cargo mid-link can leave artifacts in a bad state). A simple
"dirty" flag handles this: if changes arrive during a build, set dirty; when
build finishes, if dirty, build again.

### What about the rename on subsequent builds?

After the first rename, the running binary is at `.prev` and the cargo output
is the canonical path. If a second build triggers (e.g. dirty flag), we need
to rename the NEW binary to .prev before building again. But the current
process is still running from the FIRST .prev. Overwriting .prev with the
second-generation binary loses the original backup.

This is fine. The .prev file is always "the binary that was at the cargo output
path when the build started." If the second build fails, we restore the second-
generation binary. The first-generation binary (the one actually running) is
in memory and doesn't need the file. The only scenario where we'd need the
original .prev is if the user needs to manually restart -- and they can just
rebuild from known-good source.

### What does the user see in the browser?

1. Agent finishes its run (if one was active)
2. SSE connections drop (old server exiting)
3. Brief moment where requests fail (~500ms)
4. New server starts, binds port
5. Browser's native EventSource auto-reconnects (built into the SSE spec)
6. Frontend re-fetches session state from the new server
7. Everything looks the same -- session data is on disk

If we want polish, the frontend could show a "server restarting" toast when
SSE drops and auto-dismiss when it reconnects.

### How does the watcher know which directories to watch?

`CARGO_MANIFEST_DIR` is baked in at compile time via `env!()`. We already
use it for the frontend path. From it we derive:
- `{CARGO_MANIFEST_DIR}/src/` -- ri-web source
- `{CARGO_MANIFEST_DIR}/../ri-core/crates/` -- path dependencies
- `{CARGO_MANIFEST_DIR}/../Cargo.toml` -- workspace manifest
- `{CARGO_MANIFEST_DIR}/../Cargo.lock` -- dependency lock

### Ordering: spawn then exit, or exit then spawn?

Spawn first, then exit. If spawn fails, we log the error and keep running.

But the port must be free before the new process tries to bind. So:
1. Cancel the axum graceful shutdown token (stops accepting, drains HTTP)
2. Await the serve future returning (listener is dropped, port is free)
3. Spawn the new process
4. Exit

This guarantees the port is free before the new process tries to bind.

### Could another process grab the port in the gap?

Practically never on a dev machine. `SO_REUSEADDR` (set by tokio by default)
ensures immediate reuse. The gap is milliseconds.

### What if `current_exe()` returns the .prev path?

It might, depending on the OS. On Linux, `/proc/self/exe` follows the rename.
On macOS, `_NSGetExecutablePath` returns the original path. On Windows, the
path updates with the rename.

It doesn't matter. We never use `current_exe()` for spawning. We derive the
cargo output path from `CARGO_MANIFEST_DIR` at compile time. That's always
`target/{profile}/ri-web` regardless of renames.


## What We're NOT Doing

**exec()**: Unix-only. Spawn-and-exit works on all platforms with one code
path and the same observable behavior.

**Socket fd passing**: Adds complexity for marginal benefit. The sub-second
port gap is not worth the engineering.

**Generational spawn (Nginx model)**: Old process spawns new process, passes
socket, drains in parallel. In practice, clients must reconnect to the new
process regardless (in-memory state is gone). Drain-then-replace is simpler.

**Automatic rollback on runtime crash**: Would require an outer supervisor
process, which contradicts the single-binary design. The build gate catches
nearly everything. In Rust, if it compiles, it starts. The `.prev` binary
handles the rare exception manually.

**Preflight check**: A shadow code path that diverges from real startup --
exactly the kind of test/production divergence that erodes trust. The build
passing is sufficient validation.

**Blocking new runs during drain**: Adds UX complexity for minimal benefit.
Letting runs finish naturally is simpler and always correct.

**Alternate `--target-dir`**: Building to a separate directory to avoid
Windows locking. The rename-before-build pattern solves this more elegantly
with one operation that also serves as the backup mechanism.
