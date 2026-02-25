import { createSignal, onCleanup, Show } from 'solid-js';
import { NavState } from './types';
import SessionList from './components/SessionList';
import ChatView from './components/ChatView';
import LogPanel from './components/LogPanel';

// -- History-backed navigation --
// The browser URL stays at "/", but pushState carries a NavState object.
// This gives us back-button support without a router library.

function readNavState(): NavState {
  const s = history.state as NavState | null;
  if (s && s.view === 'session' && typeof s.id === 'string') return s;
  return { view: 'list' };
}

export default function App() {
  const [nav, setNav] = createSignal<NavState>(readNavState());
  const [logsOpen, setLogsOpen] = createSignal(false);

  // Navigate forward (user action). Pushes onto the history stack.
  const navigate = (next: NavState) => {
    history.pushState(next, '');
    setNav(next);
  };

  // Back-button: popstate fires when the user hits back/forward.
  const onPopState = () => setNav(readNavState());
  window.addEventListener('popstate', onPopState);
  onCleanup(() => window.removeEventListener('popstate', onPopState));

  // Seed the initial state so refreshing on a session view works.
  // replaceState avoids adding a duplicate entry.
  history.replaceState(nav(), '');

  const selectedId = () => {
    const n = nav();
    return n.view === 'session' ? n.id : null;
  };

  return (
    <div class={`app ${logsOpen() ? 'app-split' : ''}`}>
      {/* Main content: session list or chat view */}
      <div class="app-main">
        {/* keyed Show: forces ChatView to destroy/recreate when session id
            changes (e.g. navigating from parent to sub-session). Without keyed,
            SolidJS would keep the old component alive and just update the prop,
            but ChatView's one-shot loadSession() wouldn't re-run. */}
        <Show when={selectedId()} keyed fallback={
          <SessionList
            onSelect={(id) => navigate({ view: 'session', id })}
            logsOpen={logsOpen()}
            onToggleLogs={() => setLogsOpen(!logsOpen())}
          />
        }>
          {(id) => (
            <ChatView
              sessionId={id}
              onBack={() => history.back()}
              onNavigate={(targetId) => navigate({ view: 'session', id: targetId })}
              logsOpen={logsOpen()}
              onToggleLogs={() => setLogsOpen(!logsOpen())}
            />
          )}
        </Show>
      </div>

      {/* Log panel: global, persists across view switches */}
      <Show when={logsOpen()}>
        <LogPanel onClose={() => setLogsOpen(false)} />
      </Show>
    </div>
  );
}
