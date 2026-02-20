import { createSignal, createResource, createEffect, onCleanup, For, Show } from 'solid-js';
import { getSession, sendMessage, cancelSession, connectSSE, getSettings, updateSettings } from '../api';
import { Usage } from '../types';
import MessageView from './MessageView';

const THINKING_LEVELS = ['off', 'low', 'medium', 'high', 'xhigh'] as const;
type ThinkingLevel = typeof THINKING_LEVELS[number];

const THINKING_LABELS: Record<ThinkingLevel, string> = {
  off: 'off',
  low: 'low',
  medium: 'med',
  high: 'high',
  xhigh: 'max',
};

interface ChatViewProps {
  sessionId: string;
  onBack: () => void;
}

export default function ChatView(props: ChatViewProps) {
  const [session, { refetch }] = createResource(() => props.sessionId, getSession);
  const [messageText, setMessageText] = createSignal('');
  const [sending, setSending] = createSignal(false);
  const [streamingText, setStreamingText] = createSignal('');
  const [streamingThinking, setStreamingThinking] = createSignal('');
  const [isStreaming, setIsStreaming] = createSignal(false);
  const [usage, setUsage] = createSignal<Usage | null>(null);
  const [thinking, setThinking] = createSignal<ThinkingLevel>('medium');

  // Load initial thinking level from server settings.
  getSettings().then(s => {
    if (THINKING_LEVELS.includes(s.thinking as ThinkingLevel)) {
      setThinking(s.thinking as ThinkingLevel);
    }
  }).catch(() => {});

  let messagesEl!: HTMLDivElement;
  let textareaEl!: HTMLTextAreaElement;
  let eventSource: EventSource | null = null;

  const scrollToBottom = () => {
    if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
  };

  const resizeTextarea = () => {
    if (!textareaEl) return;
    textareaEl.style.height = 'auto';
    textareaEl.style.height = Math.min(textareaEl.scrollHeight, 200) + 'px';
  };

  // SSE connection
  createEffect(() => {
    if (props.sessionId) {
      eventSource?.close();
      eventSource = connectSSE(props.sessionId, {
        text_start: () => {
          setIsStreaming(true);
          setStreamingText('');
        },
        text_delta: (data) => {
          setStreamingText(prev => prev + data.delta);
          scrollToBottom();
        },
        thinking_start: () => { setStreamingThinking(''); },
        thinking_delta: (data) => {
          setStreamingThinking(prev => prev + data.delta);
          scrollToBottom();
        },
        message_complete: async () => {
          setIsStreaming(false);
          try { await refetch(); }
          finally {
            setStreamingText('');
            setStreamingThinking('');
            scrollToBottom();
          }
        },
        usage: (data) => { setUsage(data); },
        done: () => {
          setIsStreaming(false);
          refetch();
        },
        error: () => { setIsStreaming(false); },
        resync: () => { refetch(); },
      });
    }
  });

  onCleanup(() => { eventSource?.close(); });

  // Auto-scroll when session loads
  createEffect(() => {
    if (session()) setTimeout(scrollToBottom, 50);
  });

  const handleSend = async (e: Event) => {
    e.preventDefault();
    const text = messageText().trim();
    if (!text || sending()) return;

    setSending(true);
    try {
      await sendMessage(props.sessionId, text);
      setMessageText('');
      if (textareaEl) textareaEl.style.height = 'auto';
      refetch();
    } catch (err) {
      console.error('Send failed:', err);
    } finally {
      setSending(false);
    }
  };

  const handleCancel = async () => {
    try { await cancelSession(props.sessionId); }
    catch (err) { console.error('Cancel failed:', err); }
  };

  const cycleThinking = async () => {
    const idx = THINKING_LEVELS.indexOf(thinking());
    const next = THINKING_LEVELS[(idx + 1) % THINKING_LEVELS.length];
    setThinking(next);
    try { await updateSettings({ thinking: next }); }
    catch (err) { console.error('Settings update failed:', err); }
  };

  const isRunning = () => session()?.status === 'running' || isStreaming();

  return (
    <div class="chat-view">
      <header class="chat-header">
        <button class="back" onclick={props.onBack}>{'\u2190'}</button>
        <span class="name">{session()?.name || '...'}</span>
        <span class={`status ${session()?.status || ''}`}>
          {session()?.status || ''}
        </span>
        <Show when={isRunning()}>
          <button class="danger" onclick={handleCancel}>Cancel</button>
        </Show>
      </header>

      <div class="messages" ref={messagesEl}>
        <Show when={session.loading}><div class="loading">Loading...</div></Show>
        <Show when={session.error}><div class="error-text">Failed to load session</div></Show>

        <For each={session()?.messages}>
          {(message) => <MessageView message={message} />}
        </For>

        {/* Streaming preview */}
        <Show when={streamingText() || streamingThinking()}>
          <div class="msg streaming">
            <div class="msg-meta">
              <span class="msg-role role-asst">asst</span>
              <span class="streaming-label">streaming</span>
            </div>
            <div class="msg-body">
              <Show when={streamingThinking()}>
                <div class="collapsible">
                  <div class="collapsible-header">
                    <span class="collapsible-chevron">{'\u25BE'}</span>
                    <span class="collapsible-label">thinking</span>
                  </div>
                  <div class="collapsible-body">
                    <pre>{streamingThinking()}<span class="cursor">|</span></pre>
                  </div>
                </div>
              </Show>
              <Show when={streamingText()}>
                <div class="md-text">
                  {streamingText()}<span class="cursor">|</span>
                </div>
              </Show>
            </div>
          </div>
        </Show>
      </div>

      <form class="chat-input" onSubmit={handleSend}>
        <button
          type="button"
          class={`thinking-btn thinking-${thinking()}`}
          onclick={cycleThinking}
          title={`Thinking: ${thinking()}`}
        >{THINKING_LABELS[thinking()]}</button>
        <textarea
          ref={textareaEl}
          placeholder="Message..."
          value={messageText()}
          onInput={(e) => {
            setMessageText(e.currentTarget.value);
            resizeTextarea();
          }}
          disabled={sending() || isRunning()}
          rows="1"
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              handleSend(e);
            }
          }}
        />
        <button
          type="submit"
          class="primary"
          disabled={sending() || isRunning() || !messageText().trim()}
        >Send</button>
      </form>

      <div class="chat-footer">
        <span>{session()?.cwd || ''}</span>
        <Show when={usage()}>
          <span>{usage()!.input_tokens}in / {usage()!.output_tokens}out</span>
        </Show>
      </div>
    </div>
  );
}
