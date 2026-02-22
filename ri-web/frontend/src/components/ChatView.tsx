import { createSignal, createResource, createEffect, createMemo, onCleanup, onMount, For, Show } from 'solid-js';
import { getSession, sendMessage, cancelSession, connectSSE, getSettings, getModels, ModelInfo } from '../api';
import { marked } from 'marked';
import { Message, Usage, DisplayMode } from '../types';
import MessageView, { ToolResultInfo } from './MessageView';

const THINKING_LEVELS = ['off', 'low', 'medium', 'high', 'xhigh'] as const;
type ThinkingLevel = typeof THINKING_LEVELS[number];

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'k';
  return n.toString();
}

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
        thinking: (m.meta as any)?.thinking,
      };
    }
  }
  return {};
}

export default function ChatView(props: ChatViewProps) {
  const [session, { refetch }] = createResource(() => props.sessionId, getSession);
  const [messageText, setMessageText] = createSignal('');
  const [sending, setSending] = createSignal(false);
  const [streamingText, setStreamingText] = createSignal('');
  const [streamingThinking, setStreamingThinking] = createSignal('');
  const [isStreaming, setIsStreaming] = createSignal(false);
  const [usage, setUsage] = createSignal<Usage | null>(null);

  // Per-session settings, seeded from last successful message or server defaults.
  const [model, setModel] = createSignal<string>('');
  const [thinking, setThinking] = createSignal<ThinkingLevel>('medium');
  const [models, setModels] = createSignal<ModelInfo[]>([]);
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const [defaultsLoaded, setDefaultsLoaded] = createSignal(false);
  const [displayMode, setDisplayMode] = createSignal<DisplayMode>('compact');

  // Build a lookup from toolUseId -> result info, derived from all messages.
  // In compact mode, tool_use blocks in assistant messages will look up their
  // corresponding tool_result to show a merged single-line view.
  const toolResults = createMemo(() => {
    const map = new Map<string, ToolResultInfo>();
    for (const msg of session()?.messages || []) {
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

  // Load available models and server defaults once.
  onMount(() => {
    getModels().then(setModels).catch(() => {});
    getSettings().then(s => {
      // Only apply server defaults if session history hasn't already set them.
      if (!defaultsLoaded()) {
        if (model() == '') setModel(s.default_model);
        if (THINKING_LEVELS.includes(s.default_thinking as ThinkingLevel)) {
          setThinking(s.default_thinking as ThinkingLevel);
        }
      }
    }).catch(() => {});
  });

  // Seed settings from session history on first load only.
  let initialLoadDone = false;
  createEffect(() => {
    const s = session();
    if (!s || initialLoadDone) return;
    initialLoadDone = true;
    const prev = lastSuccessfulSettings(s.messages);
    if (prev.model) setModel(prev.model);
    if (prev.thinking && THINKING_LEVELS.includes(prev.thinking as ThinkingLevel)) {
      setThinking(prev.thinking as ThinkingLevel);
    }
    setDefaultsLoaded(true);
  });

  let messagesEl!: HTMLDivElement;
  let textareaEl!: HTMLTextAreaElement;
  let settingsRef!: HTMLDivElement;
  let eventSource: EventSource | null = null;

  const scrollToBottom = () => {
    if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
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
  onMount(() => document.addEventListener('mousedown', handleClickOutside));
  onCleanup(() => document.removeEventListener('mousedown', handleClickOutside));

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
        message_complete: async (msg: Message) => {
          setIsStreaming(false);
          // Update settings from the just-completed message.
          if (msg.role === 'assistant' && msg.provenance) {
            const hasError = msg.content?.some((b: any) => b.type === 'error');
            if (!hasError) {
              if (msg.provenance.model) setModel(msg.provenance.model);
              const msgThinking = (msg.meta as any)?.thinking;
              if (msgThinking && THINKING_LEVELS.includes(msgThinking as ThinkingLevel)) {
                setThinking(msgThinking as ThinkingLevel);
              }
            }
          }
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
        agent_error: () => {
          setIsStreaming(false);
          refetch();
        },
        resync: () => { refetch(); },
        error: () => { setIsStreaming(false); },
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
      await sendMessage(props.sessionId, text, model() || undefined, thinking());
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

  const cycleThinking = () => {
    const idx = THINKING_LEVELS.indexOf(thinking());
    const next = THINKING_LEVELS[(idx + 1) % THINKING_LEVELS.length];
    setThinking(next);
  };

  const isRunning = () => session()?.status === 'running' || isStreaming();

  // Raw model id for display.
  const modelDisplay = () => model() || '?';

  // Context window for the current model.
  const contextWindow = () => {
    const m = models().find(x => x.id === model());
    return m?.context_window || 0;
  };

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
        {/* Display mode toggle: compact (default) or debug (full message view) */}
        <button
          class={`display-mode-btn mode-${displayMode()}`}
          onclick={() => setDisplayMode(m => m === 'compact' ? 'debug' : 'compact')}
          title={`Display: ${displayMode()}`}
        >{displayMode()}</button>
      </header>

      <div class="messages" ref={messagesEl}>
        <Show when={session.loading}><div class="loading">Loading...</div></Show>
        <Show when={session.error}><div class="error-text">Failed to load session</div></Show>

        <For each={session()?.messages}>
          {(message) => <MessageView message={message} mode={displayMode()} toolResults={toolResults()} cwd={session()?.cwd || ''} />}
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
                    <div class="md-text streaming-cursor" innerHTML={marked(streamingThinking()) as string} />
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
                  class={`thinking-btn thinking-${thinking()}`}
                  onclick={cycleThinking}
                >{thinking()}</button>
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
        <span>{session()?.cwd || ''}</span>
        <span class="footer-model">{modelDisplay()}</span>
        <span class="footer-thinking">{thinking()}</span>
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
