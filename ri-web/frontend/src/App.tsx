import { createSignal, onCleanup, Show } from 'solid-js';
import { NavState } from './types';
import { connectGlobalSSE, SessionDoneEvent } from './api';
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

// -- Desktop notifications --
// Fires a browser notification when an agent loop finishes, unless the user
// is already looking at that session. Suppressed for sub-agent sessions
// (those with a parent) since those are background work.
//
// Permission must be requested from a user gesture (click/tap). We do this
// in ChatView's handleSend -- by the time the user sends their first message,
// they clearly intend to use the tool and would benefit from notifications.

function notifySessionDone(event: SessionDoneEvent, activeSessionId: string | null) {
  // Only notify if the user has opted in via the settings toggle.
  if (localStorage.getItem('ri-notifications') !== 'on') {
    console.log('Skipping notification: user opted out');
    return;
  }

  // Sub-agent sessions are background work, don't notify.
  if (event.parent) {
    console.log('Skipping notification for sub-agent session:', event);
    return;
  }

  // If the user is focused on this session's page, skip -- they're already reading it.
  if (document.visibilityState === 'visible' && activeSessionId === event.session_id) {
    console.log('Skipping notification: user is already looking at this session: ', event, document.visibilityState, activeSessionId);
    return;
  }

  if ('Notification' in window && Notification.permission === 'granted') {
    const title = event.name || 'ri';
    const body = event.preview ?? 'Agent finished';
    console.log("sending notify ", event, body);
    new Notification(title, { body, icon: '/logo/ri-128.png' });
  }
}

export default function App() {
  const [nav, setNav] = createSignal<NavState>(readNavState());
  const [logsOpen, setLogsOpen] = createSignal(false);
  const [updateAvailable, setUpdateAvailable] = createSignal(false);

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

  // -- Global SSE: session completion notifications + update availability --
  // Single connection from the root component, lives for the lifetime of the app.
  const globalSSE = connectGlobalSSE({
    onSessionDone: (event: SessionDoneEvent) => {
      notifySessionDone(event, selectedId());
    },
    onUpdateAvailable: () => {
      setUpdateAvailable(true);
    },
  });
  onCleanup(() => globalSSE.close());

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
            updateAvailable={updateAvailable()}
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
