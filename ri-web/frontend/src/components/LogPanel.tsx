import { createSignal, createMemo, createEffect, onCleanup, For, Show } from 'solid-js';
import { connectLogSSE, LogEntry } from '../api';

const LOG_LEVELS = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'] as const;
type LogLevel = typeof LOG_LEVELS[number];

const MAX_ENTRIES = 50_000;

/** Max entries rendered in the DOM at once. The full 50k buffer is kept
 *  for search/filter, but we only render the tail to avoid killing the browser. */
const MAX_RENDERED = 5_000;

/** Level ordering for "at or above" filtering. */
const LEVEL_ORD: Record<LogLevel, number> = {
  TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4,
};

const LEVEL_CLASS: Record<LogLevel, string> = {
  TRACE: 'log-trace', DEBUG: 'log-debug', INFO: 'log-info',
  WARN: 'log-warn', ERROR: 'log-error',
};

interface LogPanelProps {
  onClose: () => void;
}

/**
 * Live tracing log viewer panel.
 *
 * Connects to the global /api/logs SSE stream and accumulates entries
 * in a capped array (50k). Provides level and text filters. Auto-scrolls
 * to bottom when the user is "following" (scrolled near the end), and
 * stops when they scroll up to read.
 *
 * Only the last 5k filtered entries are rendered to keep the DOM fast.
 */
export default function LogPanel(props: LogPanelProps) {
  const [entries, setEntries] = createSignal<LogEntry[]>([]);
  const [minLevel, setMinLevel] = createSignal<LogLevel>('DEBUG');
  const [search, setSearch] = createSignal('');
  const [following, setFollowing] = createSignal(true);
  const [paused, setPaused] = createSignal(false);

  // SSE connection -- connects once, accumulates entries.
  let eventSource: EventSource | null = null;

  // Buffer incoming entries when paused so we can resume without gaps.
  let pauseBuffer: LogEntry[] = [];

  const connect = () => {
    eventSource = connectLogSSE((entry) => {
      if (paused()) {
        if (pauseBuffer.length < MAX_ENTRIES) pauseBuffer.push(entry);
        return;
      }
      setEntries(prev => {
        const next = [...prev, entry];
        return next.length > MAX_ENTRIES ? next.slice(next.length - MAX_ENTRIES) : next;
      });
    });
  };

  connect();
  onCleanup(() => eventSource?.close());

  // Flush pause buffer when unpausing.
  createEffect(() => {
    if (!paused() && pauseBuffer.length > 0) {
      const flushed = pauseBuffer.splice(0);
      setEntries(prev => {
        const next = [...prev, ...flushed];
        return next.length > MAX_ENTRIES ? next.slice(next.length - MAX_ENTRIES) : next;
      });
    }
  });

  // Memoized filtered entries -- avoids recomputing on every access.
  const filtered = createMemo(() => {
    const lvl = LEVEL_ORD[minLevel()];
    const q = search().toLowerCase();
    return entries().filter(e =>
      LEVEL_ORD[e.level] >= lvl &&
      (q === '' || e.target.toLowerCase().includes(q) || e.message.toLowerCase().includes(q))
    );
  });

  // Capped view for rendering -- only the tail so we don't create 50k DOM nodes.
  const rendered = createMemo(() => {
    const f = filtered();
    return f.length > MAX_RENDERED ? f.slice(f.length - MAX_RENDERED) : f;
  });

  // Auto-scroll: scroll to bottom when following and entries change.
  let scrollEl!: HTMLDivElement;
  createEffect(() => {
    rendered(); // track dependency
    if (following() && scrollEl) {
      requestAnimationFrame(() => {
        scrollEl.scrollTop = scrollEl.scrollHeight;
      });
    }
  });

  return (
    <div class="log-panel">
      {/* Header: title + controls + close */}
      <div class="log-panel-header">
        <span class="log-panel-title">Tracing</span>
        <span class="log-panel-count">{filtered().length}/{entries().length}</span>

        {/* Level filter */}
        <select
          class="log-level-select"
          value={minLevel()}
          onChange={(e) => setMinLevel(e.currentTarget.value as LogLevel)}
        >
          <For each={[...LOG_LEVELS]}>
            {(lvl) => <option value={lvl}>{lvl}</option>}
          </For>
        </select>

        {/* Text search */}
        <input
          class="log-search"
          type="text"
          placeholder="filter..."
          value={search()}
          onInput={(e) => setSearch(e.currentTarget.value)}
        />

        {/* Pause/resume */}
        <button
          class={`log-panel-btn ${paused() ? 'log-paused' : ''}`}
          onclick={() => setPaused(!paused())}
          title={paused() ? 'Resume' : 'Pause'}
        >{paused() ? '\u25B6' : '\u23F8'}</button>

        {/* Clear */}
        <button
          class="log-panel-btn"
          onclick={() => setEntries([])}
          title="Clear"
        >{'\u2715'}</button>

        {/* Close */}
        <button
          class="log-panel-btn"
          onclick={props.onClose}
          title="Close panel"
        >{'\u2190'}</button>
      </div>

      {/* Log entries */}
      <div
        class="log-entries"
        ref={(el) => {
          scrollEl = el;
          el.addEventListener('scroll', () => {
            setFollowing(el.scrollHeight - el.scrollTop - el.clientHeight < 40);
          }, { passive: true });
        }}
      >
        {/* Truncation notice when filtered results exceed render cap */}
        <Show when={filtered().length > MAX_RENDERED}>
          <div class="log-truncated">
            showing last {MAX_RENDERED} of {filtered().length} matches
          </div>
        </Show>
        <Show when={rendered().length === 0}>
          <div class="log-empty">
            {entries().length === 0 ? 'Waiting for log events...' : 'No entries match filter'}
          </div>
        </Show>
        <For each={rendered()}>
          {(entry) => (
            <div class={`log-line ${LEVEL_CLASS[entry.level]}`}>
              <span class="log-ts">{entry.ts}</span>
              <span class={`log-level ${LEVEL_CLASS[entry.level]}`}>{entry.level.padEnd(5)}</span>
              <span class="log-target">{entry.target}</span>
              <span class="log-msg">{entry.message}</span>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
