import { createSignal, createEffect, createMemo, onCleanup, For, Show } from 'solid-js';
import { createStore, reconcile } from 'solid-js/store';
import { getSession, sendMessage, cancelSession, connectSSE, getSettings, getModels, ModelInfo } from '../api';
import { marked } from 'marked';
import { Message, Usage, DisplayMode, fmtTokens } from '../types';
import MessageView, { ToolResultInfo } from './MessageView';

const THINKING_LEVELS = ['off', 'low', 'medium', 'high', 'xhigh'] as const;
type ThinkingLevel = typeof THINKING_LEVELS[number];

interface ChatViewProps {
  sessionId: string;
  onBack: () => void;
}

/// Scan messages in reverse for the last successful assistant message.
/// "Successful" = assistant message with no error content blocks.
/// Returns model from provenance, thinking from meta.
function lastSuccessfulSettings(messages: Message[]): { model?: string; thinking?: string } {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role !== 'assistant' || !m.provenance) continue;
    const hasError = m.content.some(b => b.type === 'error');
    if (!hasError) {
      return {
        model: m.provenance.model,
        thinking: m.meta?.thinking as string | undefined,
      };
    }
  }
  return {};
}

// -- Session store shape --
// Populated once by initial fetch, then updated incrementally by SSE events.
// Messages are keyed by id, so reconcile preserves store proxies and <For>
// keeps existing components alive (preserving foldout state, scroll, etc).
interface SessionState {
  name: string;
  cwd: string;
  status: 'idle' | 'running';
  messages: Message[];
  loading: boolean;
  error: string | null;
}

export default function ChatView(props: ChatViewProps) {
  // -- Session data store --
  const [store, setStore] = createStore<SessionState>({
    name: '',
    cwd: '',
    status: 'idle',
    messages: [],
    loading: true,
    error: null,
  });

  // Full session fetch. Used on initial load and resync (missed SSE events).
  // reconcile({ key: 'id' }) diffs messages by their stable id, preserving
  // existing store proxies so <For> keeps components (and their local state) alive.
  async function loadSession() {
    setStore('loading', true);
    setStore('error', null);
    try {
      const data = await getSession(props.sessionId);
      setStore({ name: data.name, cwd: data.cwd, status: data.status, loading: false });
      // Merge: keep any messages already in store (from SSE) that arrived
      // during the fetch. This handles the race where SSE delivers a message
      // while the GET is in flight and the response doesn't include it yet.
      const fetchedIds = new Set(data.messages.map(m => m.id));
      const extra = store.messages.filter(m => !fetchedIds.has(m.id));
      setStore('messages', reconcile([...data.messages, ...extra], { key: 'id' }));
    } catch (e) {
      setStore({ loading: false, error: String(e) });
    }
  }

  const [messageText, setMessageText] = createSignal('');
  const [sending, setSending] = createSignal(false);
  const [streamingText, setStreamingText] = createSignal('');
  const [streamingThinking, setStreamingThinking] = createSignal('');
  const [isStreaming, setIsStreaming] = createSignal(false);
  const [usage, setUsage] = createSignal<Usage | null>(null);

  // Per-session settings, seeded from last successful message or server defaults.
  const [model, setModel] = createSignal<string>('');
  const [thinking, setThinking] = createSignal<ThinkingLevel | null>(null);
  const [models, setModels] = createSignal<ModelInfo[]>([]);
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const [displayMode, setDisplayMode] = createSignal<DisplayMode>('compact');

  // Build a lookup from toolUseId -> result info, derived from all messages.
  // In compact mode, tool_use blocks in assistant messages look up their
  // corresponding tool_result to show a merged single-line view.
  const toolResults = createMemo(() => {
    const map = new Map<string, ToolResultInfo>();
    for (const msg of store.messages) {
      for (const block of msg.content) {
        if (block.type === 'tool_result') {
          map.set(block.toolUseId, {
            content: block.content,
            is_error: block.is_error,
            details: block.details
          });
        }
      }
    }
    return map;
  });

  // -- Server defaults (loaded once) --
  const [serverThinking, setServerThinking] = createSignal<ThinkingLevel>('medium');
  getModels().then(setModels).catch(() => {});
  getSettings().then(s => {
    if (THINKING_LEVELS.includes(s.default_thinking as ThinkingLevel)) {
      setServerThinking(s.default_thinking as ThinkingLevel);
    }
    // Only apply server default if session history hasn't already set model.
    if (model() === '') setModel(s.default_model);
  }).catch(() => {});

  // -- Scroll management --
  // Track whether user is "following" the conversation (scrolled near bottom)
  // or has scrolled up to read earlier content. Only auto-scroll when following.
  let messagesEl!: HTMLDivElement;
  let textareaEl!: HTMLTextAreaElement;
  let settingsRef!: HTMLDivElement;
  const [following, setFollowing] = createSignal(true);

  const scrollToBottom = () => {
    if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
  };
  const scrollIfFollowing = () => {
    if (following()) requestAnimationFrame(scrollToBottom);
  };

  const resizeTextarea = () => {
    if (!textareaEl) return;
    textareaEl.style.height = 'auto';
    textareaEl.style.height = Math.min(textareaEl.scrollHeight, 200) + 'px';
  };

  // Close settings popover on outside click.
  const handleClickOutside = (e: MouseEvent) => {
    if (settingsOpen() && settingsRef && !settingsRef.contains(e.target as Node)) {
      setSettingsOpen(false);
    }
  };
  document.addEventListener('mousedown', handleClickOutside);
  onCleanup(() => document.removeEventListener('mousedown', handleClickOutside));

  // -- SSE connection --
  // Incremental updates: message_complete appends directly to the store,
  // so <For> only creates a component for the new message. Existing
  // components (and their foldout open/closed state) are untouched.
  let eventSource: EventSource | null = null;

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
          scrollIfFollowing();
        },
        thinking_start: () => { setStreamingThinking(''); },
        thinking_delta: (data) => {
          setStreamingThinking(prev => prev + data.delta);
          scrollIfFollowing();
        },
        message_complete: (msg: Message) => {
          setIsStreaming(false);
          setStreamingText('');
          setStreamingThinking('');

          // Append to store (idempotent -- skip if already present from loadSession).
          if (!store.messages.some(m => m.id === msg.id)) {
            setStore('messages', store.messages.length, msg);
          }

          // Update settings from the just-completed assistant message.
          if (msg.role === 'assistant' && msg.provenance) {
            const hasError = msg.content?.some(b => b.type === 'error');
            if (!hasError) {
              if (msg.provenance.model) setModel(msg.provenance.model);
              const msgThinking = msg.meta?.thinking as string | undefined;
              if (msgThinking && THINKING_LEVELS.includes(msgThinking as ThinkingLevel)) {
                setThinking(msgThinking as ThinkingLevel);
              }
            }
          }

          scrollIfFollowing();
        },
        usage: (data) => { setUsage(data); },
        done: () => {
          setIsStreaming(false);
          setStore('status', 'idle');
        },
        agent_error: () => {
          setIsStreaming(false);
          // Agent may have died without sending Done. Refetch to get true state.
          // reconcile preserves existing message components.
          loadSession();
        },
        resync: () => { loadSession(); },
        error: () => { setIsStreaming(false); },
      });
    }
  });

  onCleanup(() => { eventSource?.close(); });

  // Initial load: seed settings from history, then scroll to bottom.
  // Only runs once (resync calls loadSession directly without this callback).
  loadSession().then(() => {
    const prev = lastSuccessfulSettings(store.messages);
    if (prev.model) setModel(prev.model);
    if (prev.thinking && THINKING_LEVELS.includes(prev.thinking as ThinkingLevel)) {
      setThinking(prev.thinking as ThinkingLevel);
    }
    requestAnimationFrame(scrollToBottom);
  });

  const handleSend = async (e: Event) => {
    e.preventDefault();
    const text = messageText().trim();
    if (!text || sending()) return;

    setSending(true);
    setStore('error', null);
    try {
      await sendMessage(props.sessionId, text, model() || undefined, thinking() || undefined);
      setMessageText('');
      if (textareaEl) textareaEl.style.height = 'auto';
      // Agent loop is now running. User message will arrive via SSE message_complete.
      setStore('status', 'running');
    } catch (err) {
      setStore('error', `Send failed: ${err}`);
    } finally {
      setSending(false);
    }
  };

  const handleCancel = async () => {
    try { await cancelSession(props.sessionId); }
    catch (err) { setStore('error', `Cancel failed: ${err}`); }
  };

  // Effective thinking: user/history override, or server default.
  const effectiveThinking = () => thinking() ?? serverThinking();

  const cycleThinking = () => {
    const current = effectiveThinking();
    const idx = THINKING_LEVELS.indexOf(current);
    const next = THINKING_LEVELS[(idx + 1) % THINKING_LEVELS.length];
    setThinking(next);
  };

  const isRunning = () => store.status === 'running' || isStreaming();

  // Raw model id for display.
  const modelDisplay = () => model() || '?';

  // Context window for the current model.
  const contextWindow = () => {
    const m = models().find(x => x.id === model());
    return m?.context_window || 0;
  };

  const copySessionId = (el: HTMLElement) => {
    navigator.clipboard.writeText(props.sessionId);
    const orig = el.textContent;
    el.textContent = 'copied!';
    setTimeout(() => { el.textContent = orig; }, 800);
  };

  return (
    <div class="chat-view">
      <header class="chat-header">
        <button class="back" onclick={props.onBack}>{'\u2190'}</button>
        <span class="name">{store.name || '...'}</span>
        <span class="session-id" onclick={(e) => copySessionId(e.currentTarget)} title="Click to copy">{props.sessionId}</span>
        <span class={`status ${store.status}`}>
          {store.status}
        </span>
        <Show when={isRunning()}>
          <button class="danger" onclick={handleCancel}>Cancel</button>
        </Show>
        {/* Display mode toggle: compact (default) or debug (full message view) */}
        <button
          class={`display-mode-btn mode-${displayMode()}`}
          onclick={() => setDisplayMode(m => m === 'compact' ? 'debug' : 'compact')}
          title={`Display: ${displayMode()}`}
        >{displayMode()}</button>
      </header>

      <div class="messages" ref={(el) => {
        messagesEl = el;
        // Track scroll position: following = near bottom, not following = scrolled up.
        el.addEventListener('scroll', () => {
          setFollowing(el.scrollHeight - el.scrollTop - el.clientHeight < 80);
        }, { passive: true });
      }}>
        <Show when={store.loading}><div class="loading">Loading...</div></Show>
        <Show when={store.error}><div class="error-text">{store.error}</div></Show>

        <For each={store.messages}>
          {(message) => <MessageView message={message} mode={displayMode()} toolResults={toolResults()} cwd={store.cwd} />}
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
                    <div class="md-text md-thinking streaming-cursor" innerHTML={marked(streamingThinking()) as string} />
                  </div>
                </div>
              </Show>
              <Show when={streamingText()}>
                <div class="md-text streaming-cursor" innerHTML={marked(streamingText()) as string} />
              </Show>
            </div>
          </div>
        </Show>
      </div>

      <form class="chat-input" onSubmit={handleSend}>
        <div class="settings-anchor" ref={settingsRef}>
          <button
            type="button"
            class="settings-btn"
            onclick={() => setSettingsOpen(!settingsOpen())}
            title="Settings"
          >{'\u2699'}</button>

          <Show when={settingsOpen()}>
            <div class="settings-popover">
              <div class="settings-row">
                <label class="settings-label">Model</label>
                <select
                  class="settings-select"
                  value={model()}
                  onChange={(e) => setModel(e.currentTarget.value)}
                >
                  <For each={models()}>
                    {(m) => <option value={m.id}>{m.id}</option>}
                  </For>
                </select>
              </div>
              <div class="settings-row">
                <label class="settings-label">Thinking</label>
                <button
                  type="button"
                  class={`thinking-btn thinking-${effectiveThinking()}`}
                  onclick={cycleThinking}
                >{effectiveThinking()}</button>
              </div>
            </div>
          </Show>
        </div>

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
        <span>{store.cwd}</span>
        <span class="footer-model">{modelDisplay()}</span>
        <span class="footer-thinking">{effectiveThinking()}</span>
        <Show when={usage() && contextWindow()}>
          <span>{fmtTokens(usage()!.input_tokens)}/{fmtTokens(contextWindow())} ctx</span>
          <Show when={usage()!.cache_read_tokens > 0}>
            <span>{fmtTokens(usage()!.cache_read_tokens)} cr</span>
          </Show>
          <Show when={usage()!.cache_write_tokens > 0}>
            <span>{fmtTokens(usage()!.cache_write_tokens)} cw</span>
          </Show>
        </Show>
      </div>
    </div>
  );
}
