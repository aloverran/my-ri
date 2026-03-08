# Storage Rework: Sessions as Pointers

## Problem

Sessions are supposed to be lightweight pointers (like git branches), but the current
storage makes them owners -- each session gets its own JSONL file, and all data written
during that session goes to that file. Sub-agents create their own files. Result: 239
files, 150 of which are sub-agents (63%), many just tiny headers.

## Core Insight

A JSONL file defines a **self-contained store**. It's a text format for a serialized
pool -- messages, contexts, and session pointers all in one stream. How many files exist
and what goes in which file is an **application concern**, not a format concern. You
could serve a store over HTTP and it would work identically.

## New Format

Three line types. That's it.

### Messages (unchanged)
```json
{"msg":"2603_a1b2c3d4e5f6","role":"user","content":[{"type":"text","text":"Hello"}]}
```

### Contexts (unchanged)
```json
{"context":"2603_c1d2e3f4a5b6","messages":["2603_a1b2c3d4e5f6"],"parents":[],"meta":{}}
```

### Sessions (new -- replaces session header, head update, and title lines)
```json
{"session":"2026-03-08_071223_fix-login","head":"2603_c1d2e3f4a5b6","name":"Fix login crash","ts":"2026-03-08T07:12:23Z","cwd":"/Users/john/Projects/app"}
```

Session lines use **full snapshot** semantics -- every session line repeats all fields.
Last line per session ID wins. This is simple, unambiguous, and easy to diagnose by
reading the raw file.

### What's gone

- `{"session":"name","ts":"..."}` header line (was: "this file IS this session")
- `{"head":"c1"}` anonymous head updates (was: implicitly updating "the" session)
- `{"title":"Fix login"}` separate title updates (now: just a field on the session line)

## IDs

**Session IDs**: Keep the current timestamp + slug format. Already human-readable
and practically unique: `2026-03-08_071223_fix-login`.

**Message and Context IDs**: `{YYMM}_{12 hex from UUID}`. Example: `2603_a1b2c3d4e5f6`.
17 chars total. The month prefix gives temporal context at a glance. 48 bits of
randomness is collision-safe to 200k objects (20x current scale). No file scanning
needed -- globally unique by construction.

## Pool

The Pool gains sessions as a first-class type:

```rust
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
    sessions: HashMap<SessionId, Session>,  // new
}

pub struct Session {
    pub id: SessionId,        // unique identifier (e.g. "2026-03-08_fix-login")
    pub name: String,         // display name (e.g. "Fix login crash")
    pub head: ContextId,      // current context pointer
    pub ts: String,           // creation timestamp
    pub cwd: Option<String>,
    pub parent: Option<SessionId>,
    pub file: String,         // file stem this session writes to
}
```

Three object types, one pool. The pool IS the in-memory representation of a store
file. Loading a file populates the pool. Writing objects appends to a file.

`resolve_file()` is strict: if a session isn't in the pool, it errors rather than
falling back to using the session ID as a file stem. This prevents silent creation
of orphan files (a totality violation).

## Store API

The key API change: write operations take a **file target**, not a session ID. Which
file to write to is the caller's decision.

```
write_message(file, role, content, meta) -> Message
write_context(file, messages, parents, meta) -> Context
write_session(file, session) -> ()      // full snapshot append
update_head(session_id, context_id)     // writes a session line to the appropriate file
```

`checkpoint` becomes: write_context + update_head. Same as before, just clearer.

## Application Behavior (ri-web)

The application decides file organization:

- **New top-level conversation**: creates a new file, named after the session
- **Sub-agent via runAgent**: writes to the **same file** as the parent session.
  Creates its own session pointer in that file. No new file.
- **Listing sessions**: scan all files, collect session lines, last-per-ID wins
- **Deleting**: delete the file. All sessions pointing into it are gone. Clean.

## Backward Compatibility

The loader handles old format transparently:

- `{"session":"name","ts":"..."}` → treat as origin session for legacy head lines
- `{"head":"c1"}` → map to `{"session": <origin>, "head": "c1"}` internally
- `{"title":"Fix login"}` → update the origin session's name field
- `{"msg":...}` and `{"context":...}` / `{"step":...}` → unchanged

Old files load correctly. New writes use the new format. No migration needed.

## What This Enables

- Sessions are truly just pointers. Create 100 of them, costs nothing.
- Sub-agents share the parent's file. 150 sub-agent files become 0 new files.
- The format is portable -- it's just text. Serve it over HTTP, load it from anywhere.
- File naming is convention, not coupling.
- Three line types, three pool types, one format. Simple and total.
