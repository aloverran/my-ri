import { createSignal, createResource, For, Show } from 'solid-js';
import { getSessions, createSession } from '../api';
import { SessionSummary } from '../types';
import SettingsPanel from './SettingsPanel';

interface SessionListProps {
  onSelect: (id: string) => void;
}

function relativeTime(ts: string): string {
  const diff = Date.now() - new Date(ts).getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'now';
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d`;
  return new Date(ts).toLocaleDateString();
}

export default function SessionList(props: SessionListProps) {
  const [sessions, { refetch }] = createResource<SessionSummary[]>(getSessions);
  const [name, setName] = createSignal('');
  const [cwd, setCwd] = createSignal('/Users/john/Projects/ri');
  const [creating, setCreating] = createSignal(false);
  const [showSettings, setShowSettings] = createSignal(false);

  const handleCreate = async (e: Event) => {
    e.preventDefault();
    const n = name().trim();
    const c = cwd().trim();
    if (!n || !c) return;

    setCreating(true);
    try {
      const result = await createSession(n, c);
      setName('');
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
          placeholder="Session name"
          value={name()}
          onInput={(e) => setName(e.currentTarget.value)}
          disabled={creating()}
        />
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
          disabled={creating() || !name().trim() || !cwd().trim()}
        >New</button>
      </form>

      <div class="sessions-body">
        <Show when={sessions.loading}><div class="loading">Loading...</div></Show>
        <Show when={sessions.error}><div class="error-text">Failed to load sessions</div></Show>
        <Show when={sessions() && sessions()!.length === 0}>
          <div class="empty">No sessions</div>
        </Show>

        <For each={sessions()}>
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
