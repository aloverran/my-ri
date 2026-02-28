import { createSignal, createResource, createMemo, For, Show } from 'solid-js';
import { getSessions, createSession } from '../api';
import { SessionSummary, relativeTime } from '../types';
import SettingsPanel from './SettingsPanel';

interface SessionListProps {
  onSelect: (id: string) => void;
  logsOpen: boolean;
  onToggleLogs: () => void;
}

export default function SessionList(props: SessionListProps) {
  const [allSessions, { refetch }] = createResource<SessionSummary[]>(getSessions);
  const [cwd, setCwd] = createSignal('/Users/john/Projects/ri');
  const [creating, setCreating] = createSignal(false);
  const [showSettings, setShowSettings] = createSignal(false);

  // Top-level sessions: those without a parent.
  const sessions = createMemo(() =>
    (allSessions() ?? []).filter(s => !s.parent)
  );

  // Sub-session count per parent id.
  const subCounts = createMemo(() => {
    const counts = new Map<string, number>();
    for (const s of allSessions() ?? []) {
      if (s.parent) counts.set(s.parent, (counts.get(s.parent) ?? 0) + 1);
    }
    return counts;
  });

  const handleCreate = async (e: Event) => {
    e.preventDefault();
    const c = cwd().trim();
    if (!c) return;

    setCreating(true);
    try {
      const result = await createSession(c);
      refetch();
      props.onSelect(result.id);
    } catch (err) {
      console.error('Create failed:', err);
    } finally {
      setCreating(false);
    }
  };

  return (
    <div class="session-list">
      <div class="session-list-header">
        <h1>ri</h1>
        <button
          class={`log-toggle-btn ${props.logsOpen ? 'log-toggle-active' : ''}`}
          onclick={props.onToggleLogs}
          title="Tracing logs"
        >log</button>
        <button
          class="session-list-settings-btn"
          onclick={() => setShowSettings(!showSettings())}
          title="Settings"
        >{'\u2699'}</button>
      </div>

      {/* Global settings panel (auth, etc) */}
      <Show when={showSettings()}>
        <SettingsPanel onClose={() => setShowSettings(false)} />
      </Show>

      <form class="new-session-form" onSubmit={handleCreate}>
        <input
          type="text"
          placeholder="Working directory"
          value={cwd()}
          onInput={(e) => setCwd(e.currentTarget.value)}
          disabled={creating()}
        />
        <button
          type="submit"
          class="primary"
          disabled={creating() || !cwd().trim()}
        >New</button>
      </form>

      <div class="sessions-body">
        <Show when={allSessions.loading}><div class="loading">Loading...</div></Show>
        <Show when={allSessions.error}><div class="error-text">Failed to load sessions</div></Show>
        <Show when={sessions().length === 0 && !allSessions.loading}>
          <div class="empty">No sessions</div>
        </Show>

        <For each={sessions()}>
          {(session) => {
            const subs = () => subCounts().get(session.id) ?? 0;
            return (
              <div class="session-row" onclick={() => props.onSelect(session.id)}>
                <div class="session-row-top">
                  <span class="session-name">{session.name}</span>
                  <div class="session-meta">
                    <span>{session.message_count} msgs</span>
                    <Show when={subs() > 0}>
                      <span class="session-sub-count">{subs()} sub</span>
                    </Show>
                    <span>{relativeTime(session.ts)}</span>
                  </div>
                </div>
                <div class="session-cwd">{session.cwd}</div>
              </div>
            );
          }}
        </For>
      </div>
    </div>
  );
}
