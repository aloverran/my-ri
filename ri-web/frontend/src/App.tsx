import { createSignal, Show } from 'solid-js';
import SessionList from './components/SessionList';
import ChatView from './components/ChatView';
import LogPanel from './components/LogPanel';

export default function App() {
  const [selectedSessionId, setSelectedSessionId] = createSignal<string | null>(null);
  const [logsOpen, setLogsOpen] = createSignal(false);

  return (
    <div class={`app ${logsOpen() ? 'app-split' : ''}`}>
      {/* Main content: session list or chat view */}
      <div class="app-main">
        {selectedSessionId() ? (
          <ChatView
            sessionId={selectedSessionId()!}
            onBack={() => setSelectedSessionId(null)}
            logsOpen={logsOpen()}
            onToggleLogs={() => setLogsOpen(!logsOpen())}
          />
        ) : (
          <SessionList
            onSelect={(id) => setSelectedSessionId(id)}
            logsOpen={logsOpen()}
            onToggleLogs={() => setLogsOpen(!logsOpen())}
          />
        )}
      </div>

      {/* Log panel: global, persists across view switches */}
      <Show when={logsOpen()}>
        <LogPanel onClose={() => setLogsOpen(false)} />
      </Show>
    </div>
  );
}
