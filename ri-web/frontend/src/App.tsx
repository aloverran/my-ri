import { createSignal } from 'solid-js';
import SessionList from './components/SessionList';
import ChatView from './components/ChatView';

export default function App() {
  const [selectedSessionId, setSelectedSessionId] = createSignal<string | null>(null);

  return (
    <div class="app">
      {selectedSessionId() ? (
        <ChatView 
          sessionId={selectedSessionId()!}
          onBack={() => setSelectedSessionId(null)}
        />
      ) : (
        <SessionList 
          onSelect={(id) => setSelectedSessionId(id)}
        />
      )}
    </div>
  );
}