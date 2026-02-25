import { createResource, createMemo, For, Show } from 'solid-js';
import { getSessions } from '../api';
import { SessionSummary, relativeTime } from '../types';

interface SubSessionsPanelProps {
  /** The parent session id to show sub-sessions for. */
  parentId: string;
  onSelect: (id: string) => void;
  onClose: () => void;
}

/**
 * Side panel listing sub-sessions of a parent session.
 * Mirrors the LogPanel layout: fixed-width right panel with a header and scrollable list.
 */
export default function SubSessionsPanel(props: SubSessionsPanelProps) {
  const [allSessions] = createResource<SessionSummary[]>(getSessions);

  const children = createMemo(() =>
    (allSessions() ?? []).filter(s => s.parent === props.parentId)
  );

  return (
    <div class="sub-panel">
      <div class="sub-panel-header">
        <span class="sub-panel-title">Sub-sessions</span>
        <span class="sub-panel-count">{children().length}</span>
        <button
          class="log-panel-btn"
          onclick={props.onClose}
          title="Close panel"
        >{'\u2190'}</button>
      </div>

      <div class="sub-panel-body">
        <Show when={allSessions.loading}>
          <div class="loading">Loading...</div>
        </Show>
        <Show when={children().length === 0 && !allSessions.loading}>
          <div class="empty">No sub-sessions</div>
        </Show>

        <For each={children()}>
          {(session) => (
            <div class="session-row" onclick={() => props.onSelect(session.id)}>
              <div class="session-row-top">
                <span class="session-name">{session.name}</span>
                <div class="session-meta">
                  <span>{session.message_count} msgs</span>
                  <span>{relativeTime(session.ts)}</span>
                </div>
              </div>
              <div class="session-cwd">{session.cwd}</div>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
