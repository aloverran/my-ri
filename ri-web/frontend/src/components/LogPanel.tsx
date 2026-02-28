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

/** Panel width constraints (px). */
const MIN_PANEL_W = 200;
const MAX_PANEL_RATIO = 0.8;
const DEFAULT_PANEL_W = 480;

/** Detail pane height constraints (px). */
const MIN_DETAIL_H = 60;
const MAX_DETAIL_RATIO = 0.6;
const DEFAULT_DETAIL_H = 200;

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
 *
 * Features:
 *  - Drag the left edge to resize the panel width.
 *  - Click a log entry to select it; a detail pane at the bottom shows
 *    the full wrapped text (like Unity's Console). The detail pane split
 *    is also draggable.
 */
export default function LogPanel(props: LogPanelProps) {
  const [entries, setEntries] = createSignal<LogEntry[]>([]);
  const [minLevel, setMinLevel] = createSignal<LogLevel>('DEBUG');
  const [search, setSearch] = createSignal('');
  const [following, setFollowing] = createSignal(true);
  const [paused, setPaused] = createSignal(false);
  const [panelWidth, setPanelWidth] = createSignal(DEFAULT_PANEL_W);
  const [selectedEntry, setSelectedEntry] = createSignal<LogEntry | null>(null);
  const [detailHeight, setDetailHeight] = createSignal(DEFAULT_DETAIL_H);

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
  onCleanup(() => {
    eventSource?.close();
    endDrag(); // Clean up body styles if unmounted mid-drag.
  });

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

  // -- Drag helpers --
  // Both resize handles use pointer capture so the mouse can leave the
  // handle element without dropping the drag. Body cursor and user-select
  // are set while dragging to prevent flicker and text selection.
  // Cleanup uses onLostPointerCapture instead of onPointerUp -- it fires
  // in every case: normal release, element removal, browser cancellation.

  const beginDrag = (cursor: string) => {
    document.body.style.cursor = cursor;
    document.body.style.userSelect = 'none';
  };

  const endDrag = () => {
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
  };

  // -- Horizontal (panel width) resize --
  let widthHandleRef!: HTMLDivElement;

  const onWidthPointerDown = (e: PointerEvent) => {
    widthHandleRef.setPointerCapture(e.pointerId);
    beginDrag('col-resize');
  };

  const onWidthPointerMove = (e: PointerEvent) => {
    if (!widthHandleRef.hasPointerCapture(e.pointerId)) return;
    const w = window.innerWidth - e.clientX;
    const clamped = Math.max(MIN_PANEL_W, Math.min(w, window.innerWidth * MAX_PANEL_RATIO));
    setPanelWidth(clamped);
  };

  // -- Vertical (detail pane height) resize --
  let detailHandleRef!: HTMLDivElement;
  let panelBodyRef!: HTMLDivElement;

  const onDetailPointerDown = (e: PointerEvent) => {
    detailHandleRef.setPointerCapture(e.pointerId);
    beginDrag('row-resize');
  };

  const onDetailPointerMove = (e: PointerEvent) => {
    if (!detailHandleRef.hasPointerCapture(e.pointerId)) return;
    const panelRect = panelBodyRef.getBoundingClientRect();
    // Subtract handle height (4px) so the handle tracks the cursor accurately.
    const h = panelRect.bottom - e.clientY - 4;
    const maxH = panelRect.height * MAX_DETAIL_RATIO;
    const clamped = Math.max(MIN_DETAIL_H, Math.min(h, maxH));
    setDetailHeight(clamped);
  };

  return (
    <div class="log-panel" style={{ width: `${panelWidth()}px` }}>
      {/* Left-edge drag handle for panel width resize */}
      <div
        class="log-panel-resize-handle"
        ref={widthHandleRef}
        onPointerDown={onWidthPointerDown}
        onPointerMove={onWidthPointerMove}
        onLostPointerCapture={endDrag}
      />

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
          onclick={() => { setEntries([]); setSelectedEntry(null); }}
          title="Clear"
        >{'\u2715'}</button>

        {/* Close */}
        <button
          class="log-panel-btn"
          onclick={props.onClose}
          title="Close panel"
        >{'\u2190'}</button>
      </div>

      {/* Panel body: log entries list + optional detail pane, split vertically */}
      <div class="log-panel-body" ref={panelBodyRef}>
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
              <div
                class={`log-line ${LEVEL_CLASS[entry.level]} ${selectedEntry() === entry ? 'log-line-selected' : ''}`}
                onClick={() => setSelectedEntry(entry)}
              >
                <span class="log-ts">{entry.ts}</span>
                <span class={`log-level ${LEVEL_CLASS[entry.level]}`}>{entry.level.padEnd(5)}</span>
                <span class="log-target">{entry.target}</span>
                <span class="log-msg">{entry.message}</span>
              </div>
            )}
          </For>
        </div>

        {/* Detail pane: shows full text of selected entry */}
        <Show when={selectedEntry()}>
          {(entry) => (
            <>
              {/* Horizontal drag handle for detail pane height */}
              <div
                class="log-detail-resize-handle"
                ref={detailHandleRef}
                onPointerDown={onDetailPointerDown}
                onPointerMove={onDetailPointerMove}
                onLostPointerCapture={endDrag}
              />
              <div class="log-detail" style={{ height: `${detailHeight()}px` }}>
                <div class={`log-detail-line ${LEVEL_CLASS[entry().level]}`}>
                  <span class="log-detail-ts">{entry().ts}</span>
                  <span class={`log-detail-level ${LEVEL_CLASS[entry().level]}`}>{entry().level}</span>
                  <span class="log-detail-target">{entry().target}</span>
                </div>
                <div class="log-detail-message">{entry().message}</div>
              </div>
            </>
          )}
        </Show>
      </div>
    </div>
  );
}
