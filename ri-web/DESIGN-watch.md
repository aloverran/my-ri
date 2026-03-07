# Design: `ri-web --watch`

Development hot-reload for ri-web. The binary becomes its own supervisor,
watching Rust source files, rebuilding on change, and providing a UI-driven
restart that never interrupts running agent sessions.

## User experience

```
$ cargo build -p ri-web && ./target/release/ri-web --watch --port 3001
```

1. Supervisor discovers all local crate source directories via `cargo metadata`.
2. Supervisor builds `ri-web` (mirroring the current profile), spawns it as a child.
3. The user works normally. Agents run, sessions persist.
4. Developer edits Rust source. The supervisor detects the change, rebuilds
   in the background. If the build succeeds and the binary actually changed,
   it writes a signal to the child's stdin.
5. In the frontend, an **"Update"** button appears in the session list header.
6. The user clicks it when ready. The child waits for all running agents to
   finish (same drain logic as ctrl-c), then exits with code **42**.
7. Supervisor sees exit code 42, spawns the newly-built binary. The frontend
   reloads the page and the new server is already up.

If the build fails, nothing happens. The old server keeps running. The
supervisor logs the error and waits for the next file change.

## Architecture: two-process, stdin IPC

```
ri-web --watch [--port P --host H ...]
  |
  |  (supervisor process -- watches files, runs cargo, manages child)
  |
  +--stdin pipe--> ri-web --port P --host H  (child -- the real server)
```

The supervisor is thin. It does not serve HTTP, does not proxy, does not
know about agents or sessions. It has three responsibilities:

1. Watch source files for changes.
2. Run `cargo build` and check if the output binary changed.
3. Manage the child process lifecycle (spawn, signal, wait, respawn).

All complex logic -- agent session management, SSE, the "update available"
UI -- lives in the child, which is the existing ri-web with small additions.

### Supervisor signal handling

The supervisor must trap `ctrl-c` to avoid orphaning the child. When
`SIGINT` arrives (the OS sends it to the entire process group), the
supervisor ignores it and waits for the child to exit naturally. The
child has its own graceful shutdown handler that drains agents.

```rust
// In the supervisor's main loop:
tokio::select! {
    status = child.wait()       => { /* child exited, check code */ }
    _ = tokio::signal::ctrl_c() => {
        // Child got SIGINT too (same process group). Just wait for it.
        let status = child.wait().await?;
        // Exit with the child's exit code.
        std::process::exit(status.code().unwrap_or(1));
    }
}
```

Without this, a ctrl-c kills the supervisor instantly while the child
continues draining in the background. The next `cargo run` would fail
with "Address already in use" because the port is still bound.

### Why not a single process?

A single process cannot replace itself portably. `exec()` does not exist
on Windows. A two-process model where the supervisor is a separate invocation
of the same binary is the standard pattern (used by `cargo-watch`, `watchexec`,
and `systemfd`). The supervisor's code is behind the `--watch` flag and
shares no runtime state with the server code.

## IPC protocol: stdin

The supervisor spawns the child with `stdin` piped (not inherited). Two
signals are transmitted over this pipe:

| Signal | Encoding | Meaning |
|--------|----------|---------|
| Update ready | `"update\n"` written to stdin | New binary built, update available |
| Shutdown | stdin EOF (pipe closed) | Supervisor died or was killed; child should shut down gracefully |

The child spawns a tokio task that reads lines from stdin. On `"update"`,
it sets an internal flag. On EOF, it initiates the same graceful shutdown
as ctrl-c.

This is the simplest cross-platform IPC. No sockets, no files, no signals.
Works identically on macOS, Linux, and Windows.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Normal exit (ctrl-c shutdown) |
| 42 | Restart requested (user clicked "Update") |
| other | Crash or error |

The supervisor's behavior:
- **42**: Spawn the new binary immediately (already built).
- **0 or other**: Exit the supervisor too. We are not a production process
  manager. Crashes mean something is wrong, and normal exits mean the user
  intended to stop.

## File watching

### Discovering watch paths

At startup, the supervisor runs:

```
cargo metadata --format-version 1 --manifest-path <workspace_root>/Cargo.toml
```

It parses the JSON output and collects every package where `source` is
`null` (local path dependency). For each, it derives `<manifest_dir>/src/`
as the directory to watch. It also watches all local `Cargo.toml` files
and the workspace-level `Cargo.lock`.

This automatically picks up any crate that ri-web depends on transitively
via path deps, including `ri-core/crates/ri/`, `ri-core/crates/ri-ai/`,
etc. If a new local crate is added, no manual path enumeration is needed.

### Debouncing

Use `notify-debouncer-mini` with a 200ms debounce window. This crate wraps
the `notify` file watcher and collapses the burst of events that a single
file save produces (especially on macOS FSEvents).

Events are bridged from `notify`'s `std::sync::mpsc` channel into a
`tokio::sync::mpsc` channel via `spawn_blocking`, so the rest of the
supervisor can be async.

### What to ignore

- `target/` directories (build artifacts would trigger infinite loops)
- `.git/` directories
- Hidden files (dotfiles)
- Non-Rust files within `src/` (shouldn't normally exist, but defensive)

Since we only watch specific `src/` directories and `Cargo.toml` files
discovered by `cargo metadata`, the ignore list is naturally narrow.

## Build process

### Profile mirroring

The binary knows its own build profile at compile time:

```rust
const IS_RELEASE: bool = !cfg!(debug_assertions);
```

The supervisor passes `--release` to `cargo build` when `IS_RELEASE` is true.
This mirrors whatever profile was used to build the supervisor itself.

### Build invocation

```rust
let mut cmd = Command::new("cargo");
cmd.args(["build", "-p", "ri-web"]);
if IS_RELEASE { cmd.arg("--release"); }
```

The build runs in the background while the old server is still serving.
By the time the user clicks "Update", the new binary is already built.

### Binary change detection

Before and after `cargo build`, the supervisor captures the **build
target's** `mtime` (filesystem modification time). The build target path
is derived from the workspace root + profile:

```rust
let exe_name = format!("ri-web{}", std::env::consts::EXE_SUFFIX);
let profile_dir = if IS_RELEASE { "release" } else { "debug" };
let exe_path = workspace_root.join("target").join(profile_dir).join(&exe_name);

let mtime_before = fs::metadata(&exe_path).and_then(|m| m.modified()).ok();
// ... run cargo build ...
let mtime_after = fs::metadata(&exe_path).and_then(|m| m.modified()).ok();
let changed = mtime_before != mtime_after;
```

Note: do NOT use `std::env::current_exe()` here. On Windows, that returns
the supervisor's shadow copy path, not the build target.

### Build-while-building

If a file change arrives while a build is already running, the supervisor
sets a `rebuild_pending` flag. When the current build finishes, it checks
the flag and starts another build. This collapses rapid changes into at
most two builds.

### Build failure

If `cargo build` exits non-zero, the supervisor logs the error (stderr
is inherited, so the user sees compiler errors in the terminal) and
does **not** signal the child. The old server keeps running.

## Windows: executable locking

On Windows, a running `.exe` is file-locked by the OS. Both the supervisor
and the child hold locks on their respective executables. If `cargo build`
wants to overwrite a file that's currently running, it fails with "Access
is denied".

### Solution: child runs from a copy

The supervisor copies the built binary to a timestamped child path before
spawning:

```rust
fn child_exe_path(built_exe: &Path) -> PathBuf {
    let stem = built_exe.file_stem().unwrap().to_string_lossy();
    let suffix = std::env::consts::EXE_SUFFIX;
    built_exe.with_file_name(format!("{}_child{}", stem, suffix))
}
```

On spawn:
1. Copy `target/{profile}/ri-web{.exe}` to `target/{profile}/ri-web_child{.exe}`
2. Spawn the copy as the child process.
3. The original `ri-web{.exe}` is unlocked -- `cargo build` can overwrite it.
4. When the child exits for an update, delete the old copy, copy the
   newly-built binary, spawn again.

On macOS and Linux this copy step is unnecessary -- the OS allows
overwriting a running binary's file (the old inode stays alive until the
process exits). But using the copy unconditionally keeps the code simpler
and the behavior identical across platforms. The copy is a ~5MB file
operation that takes <10ms.

### Supervisor executable

The supervisor itself also needs to avoid locking the build target. Two
approaches:

- **On Windows**: the supervisor is launched via `cargo run --watch`, or
  installed separately, or shadow-copied at startup (copy self to
  `ri-web_supervisor.exe`, exec the copy, exit original).
- **On macOS/Linux**: not an issue, cargo can overwrite running binaries.

Since `--watch` is a development feature and the supervisor is long-lived,
the shadow copy at startup is the simplest universal solution:

```rust
#[cfg(target_os = "windows")]
fn maybe_shadow_copy() {
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_name().unwrap().to_string_lossy();
    if !name.contains("_supervisor") {
        let shadow = exe.with_file_name(
            format!("ri-web_supervisor{}", std::env::consts::EXE_SUFFIX)
        );
        std::fs::copy(&exe, &shadow).unwrap();
        Command::new(&shadow).args(std::env::args().skip(1)).spawn().unwrap();
        std::process::exit(0);
    }
}
```

## Changes to existing ri-web

### CLI

Add `--watch` flag to the `Cli` struct. When present, `main()` branches
into `run_supervisor()` instead of the normal server startup.

### Stdin monitor (child side)

The supervisor sets `RI_WEB_SUPERVISED=1` in the child's environment.
The child checks this on startup:

```rust
let supervised = std::env::var("RI_WEB_SUPERVISED").is_ok();
```

Why not `!stdin().is_terminal()`? Because that would false-positive in
Docker, systemd, nohup, or any context where stdin is not a TTY. A pipe
to `/dev/null` would cause immediate EOF and trigger graceful shutdown
-- a totality bug. The env var is deterministic: it's only set by the
supervisor, never by accident.

When supervised, the child spawns a tokio task that reads lines from
stdin (which the supervisor has piped). On `"update"`, it sets the
`update_available` flag. On EOF, it triggers graceful shutdown.

The child also emits a `GlobalEvent::UpdateAvailable` on the global
broadcast channel when the flag transitions from false to true.

### AppState additions

```rust
pub struct AppState {
    // ... existing fields ...

    /// True when the supervisor has signaled that a new binary is ready.
    /// Set by the stdin monitor, read by the /api/update endpoint and
    /// broadcast via global SSE.
    pub update_available: AtomicBool,

    /// Signals the shutdown task to begin the update-restart sequence
    /// (exit code 42). Only populated when running as a supervised child.
    pub update_trigger: Arc<tokio::sync::Notify>,
}
```

### Shutdown orchestration (modified)

The existing shutdown task in `main()` waits on `ctrl_c()`. For supervised
mode, this task gains a second trigger: an `Arc<Notify>`.

```rust
tokio::select! {
    _ = tokio::signal::ctrl_c()     => { exit_code = 0; }
    _ = update_trigger.notified()   => { exit_code = 42; }
}

tracker.close();

tokio::select! {
    _ = tracker.wait()          => { /* all agents done */ }
    _ = tokio::signal::ctrl_c() => { std::process::exit(1); }
}

if exit_code == 42 {
    // Update path: skip graceful HTTP drain, just exit.
    // All agent work is persisted. Axum connections don't matter
    // because the server is about to restart.
    std::process::exit(42);
}

// Normal ctrl-c path: graceful HTTP drain.
shutdown.cancel();
let _ = server_stop_tx.send(());
```

### New API endpoint

```
POST /api/update
```

When called:
1. Check `update_available`. If false, return 409 Conflict.
2. Call `state.update_trigger.notify_one()`. This wakes the shutdown task,
   which closes the tracker and waits for agents to drain, then exits 42.
   Safe against double-clicks -- `notify_one()` is idempotent.

This endpoint is fire-and-forget from the frontend's perspective. The
server will go down, and the frontend knows to expect it.

### New global event

```rust
pub enum GlobalEvent {
    SessionDone { ... },
    /// New binary built and ready; frontend should show the update button.
    #[serde(rename = "update_available")]
    UpdateAvailable,
}
```

## Frontend changes

### api.ts

Update `connectGlobalSSE` to also listen for the `update_available` event.
Add a `postUpdate()` function that POSTs to `/api/update`.

### App.tsx

Add `updateAvailable` signal, set by the global SSE handler. Pass it down
to `SessionList` as a prop.

### SessionList.tsx

In the header div, conditionally show an "Update" button:

```tsx
<Show when={props.updateAvailable}>
  <button class="update-btn" onclick={handleUpdate}>
    Update
  </button>
</Show>
```

Clicking it calls `postUpdate()`. No spinner, no loading state. The server
goes down and the page stops working -- the user refreshes when the new
server is up.

### Styling

The update button should be visually distinct but not alarming. A small
accent-colored button in the header row, next to "log" and the gear icon.

## Supervisor state machine

```
                  +-------------+
                  |   STARTUP   |
                  +------+------+
                         |
                    cargo build
                         |
                  +------v------+
           +----->|   RUNNING   |<--------+
           |      +------+------+         |
           |             |                |
           |     file change detected     |
           |             |                |
           |      +------v------+         |
           |      |  BUILDING   |         |
           |      +------+------+         |
           |             |                |
           |        build result          |
           |         /        \           |
           |    success      failure      |
           |   (changed)    (or no-op)    |
           |       |              |       |
           |   write "update\n"   +-------+
           |   to child stdin     (stay in RUNNING,
           |       |               wait for next change)
           |       |
           |  +----v---------+
           |  | UPDATE READY |
           |  +----+---------+
           |       |
           |  child exits 42
           |       |
           |  spawn new child
           |       |
           +----- -+
```

If the child exits with any code other than 42 at any point, the supervisor
exits too.

## New dependencies

Add to `ri-web/Cargo.toml`:

```toml
notify-debouncer-mini = "0.7"
serde_json = "1"  # already present, for cargo metadata parsing
```

`std::io::IsTerminal` is in std since Rust 1.70, no extra crate needed.
`tokio::process` is already available via `tokio = { features = ["full"] }`.

## Code layout

```
ri-web/src/
  main.rs          -- add --watch flag, branch to supervisor or server
  watch.rs         -- NEW: supervisor logic (~150-200 lines)
                      - cargo metadata parsing
                      - file watcher setup
                      - child process management
                      - build invocation
                      - state machine
  state.rs         -- add update_available: AtomicBool
  api.rs           -- add POST /api/update endpoint
                      add UpdateAvailable to GlobalEvent
```

The supervisor code is entirely in `watch.rs`. It does not import any
ri-web server modules (no state, no agent, no api). It is a standalone
module that happens to live in the same binary.

## Edge cases

| Scenario | Behavior |
|----------|----------|
| Build fails | Old server keeps running. Error visible in terminal. No update signal. |
| Agent running when update clicked | Child waits for all agents to drain (same as ctrl-c), then exits 42. |
| Multiple file changes during build | `rebuild_pending` flag; one rebuild after current finishes. |
| Supervisor killed (SIGTERM) | Child detects stdin EOF, shuts down gracefully. |
| Ctrl-c in terminal | Supervisor traps it, waits for child to exit, then exits with child's code. |
| Child crashes | Supervisor exits with the child's exit code. |
| No file changes, user never clicks update | Nothing happens. Server runs normally forever. |
| `cargo metadata` fails | Supervisor exits with error. Not recoverable. |
| Binary unchanged after build | No update signal. mtime check catches this. |
| --watch on Windows | Shadow copy for supervisor + child copy ensures cargo can overwrite. |
| Frontend: server goes down for restart | SSE disconnects, page goes stale. User refreshes manually. |
| ri-web run via Docker/nohup (no TTY) | No false-positive shutdown. Env var detection is deterministic. |
| Double-click on Update button | `Notify` is idempotent; second POST is harmless (409 or no-op). |
