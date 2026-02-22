import { createSignal, For, Show } from 'solid-js';
import { marked } from 'marked';
import { Message, ContentBlock, DisplayMode, Usage } from '../types';
import { highlight, highlightInline, langFromPath } from '../highlight';

/** Data needed to resolve tool_use -> tool_result in compact mode. */
export interface ToolResultInfo {
  content: ContentBlock[];
  is_error: boolean;
  details?: any;
}

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + '...';
}

function toolPreview(name: string, input: unknown, cwd: string): string {
  if (!input || typeof input !== 'object') return JSON.stringify(input).slice(0, 80);
  const obj = input as Record<string, unknown>;
  switch (name) {
    case 'bash': return typeof obj.command === 'string' ? obj.command : '';
    case 'read':
    case 'write':
    case 'edit': {
      if (typeof obj.path !== 'string') return '';
      // Show path relative to cwd when possible
      const p = obj.path as string;
      if (cwd && p.startsWith(cwd + '/')) return p.slice(cwd.length + 1);
      return p;
    }
    default: return JSON.stringify(input).slice(0, 120);
  }
}

function extractText(blocks: ContentBlock[]): string {
  return blocks
    .filter((b): b is Extract<ContentBlock, { type: 'text' }> => b.type === 'text')
    .map(b => b.text)
    .join('\n');
}

function firstLine(text: string): string {
  return text.split('\n').find(l => l.trim() !== '')?.trim() || '';
}

// --- Token formatting ---

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'k';
  return n.toString();
}

function UsageStats(props: { usage: Usage; model: string }) {
  const [open, setOpen] = createSignal(false);
  const u = () => props.usage;
  const total = () => u().input_tokens + u().output_tokens;
  const cacheHitPct = () => {
    const inp = u().input_tokens + u().cache_read_tokens;
    if (inp === 0) return 0;
    return Math.round((u().cache_read_tokens / inp) * 100);
  };
  const hasExtras = () => u().extras && Object.keys(u().extras!).length > 0;
  return (
    <div class="usage-stats">
      <div class="usage-summary" onclick={() => hasExtras() && setOpen(!open())}>
        <span class="usage-model">{props.model}</span>
        <span class="usage-item">{fmtTokens(u().input_tokens)} in</span>
        <span class="usage-item">{fmtTokens(u().output_tokens)} out</span>
        <Show when={u().cache_read_tokens > 0}>
          <span class="usage-item usage-cache-read">{fmtTokens(u().cache_read_tokens)} cached ({cacheHitPct()}%)</span>
        </Show>
        <Show when={u().cache_write_tokens > 0}>
          <span class="usage-item usage-cache-write">{fmtTokens(u().cache_write_tokens)} cache-write</span>
        </Show>
        <span class="usage-total">{fmtTokens(total())} total</span>
        <Show when={hasExtras()}>
          <span class="usage-expand">{open() ? '\u25BE' : '\u25B8'}</span>
        </Show>
      </div>
      <Show when={open() && hasExtras()}>
        <div class="usage-extras">
          <For each={Object.entries(u().extras!)}>
            {([key, value]) => (
              <div class="usage-extra-row">
                <span class="usage-extra-key">{key}</span>
                <span class="usage-extra-val">
                  {typeof value === 'object' ? JSON.stringify(value) : String(value)}
                </span>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

// --- Collapsible sub-components (debug mode) ---

function ThinkingBlock(props: { text: string }) {
  const [open, setOpen] = createSignal(false);
  return (
    <div class="collapsible">
      <button class="collapsible-header" onclick={() => setOpen(!open())}>
        <span class="collapsible-chevron">{open() ? '\u25BE' : '\u25B8'}</span>
        <span class="collapsible-label">thinking</span>
        <Show when={!open()}>
          <span class="collapsible-preview">{truncate(firstLine(props.text), 60)}</span>
        </Show>
      </button>
      <Show when={open()}>
        <div class="collapsible-body"><pre>{props.text}</pre></div>
      </Show>
    </div>
  );
}

function ToolUseBlock(props: { name: string; input: unknown }) {
  const [open, setOpen] = createSignal(false);
  const preview = () => truncate(toolPreview(props.name, props.input, ''), 80);
  return (
    <div class="collapsible">
      <button class="collapsible-header" onclick={() => setOpen(!open())}>
        <span class="collapsible-chevron">{open() ? '\u25BE' : '\u25B8'}</span>
        <span class="collapsible-label">{props.name}</span>
        <span class="collapsible-preview">{preview()}</span>
      </button>
      <Show when={open()}>
        <div class="collapsible-body">
          {props.name === 'bash' && typeof (props.input as any)?.command === 'string'
            ? <div innerHTML={highlight((props.input as any).command, 'bash')} />
            : <div innerHTML={highlight(JSON.stringify(props.input, null, 2), 'json')} />
          }
        </div>
      </Show>
    </div>
  );
}

function ToolResultBlock(props: { content: ContentBlock[]; isError: boolean }) {
  const [open, setOpen] = createSignal(false);
  const text = () => extractText(props.content);
  const preview = () => truncate(firstLine(text()), 80);
  return (
    <div class="collapsible">
      <button class="collapsible-header" onclick={() => setOpen(!open())}>
        <span class="collapsible-chevron">{open() ? '\u25BE' : '\u25B8'}</span>
        <span class={`collapsible-tag ${props.isError ? 'err' : ''}`}>
          {props.isError ? 'ERR' : 'OK'}
        </span>
        <span class="collapsible-preview">{preview()}</span>
      </button>
      <Show when={open()}>
        <div class="collapsible-body"><pre>{text()}</pre></div>
      </Show>
    </div>
  );
}

function SystemMessage(props: { message: Message; mode: DisplayMode }) {
  const [open, setOpen] = createSignal(false);
  const text = () => extractText(props.message.content);
  return (
    <div class="msg-system">
      <button class="collapsible-header" onclick={() => setOpen(!open())}>
        <span class="collapsible-chevron">{open() ? '\u25BE' : '\u25B8'}</span>
        <span class="collapsible-label">system</span>
        <span class="collapsible-preview">{truncate(firstLine(text()), 60)}</span>
      </button>
      <Show when={open()}>
        <div class="collapsible-body"><pre>{text()}</pre></div>
      </Show>
      <Show when={props.mode === 'debug'}>
        <span class="msg-id">{props.message.id}</span>
      </Show>
    </div>
  );
}

function ErrorBlock(props: { message: string }) {
  return (
    <div class="error-block">
      <div class="error-header">
        <span class="error-icon">!</span>
        <span class="error-label">Error</span>
      </div>
      <div class="error-body">{props.message}</div>
    </div>
  );
}

// --- Compact merged tool call: invocation line, expand for result ---

/**
 * A single merged tool call line for compact mode.
 * Collapsed: always shows "toolname  command/path" (invocation only).
 * Expanded: shows full input + output.
 * Pending vs done distinguished by left border color + background.
 */
function CompactToolCall(props: {
  name: string;
  input: unknown;
  result: ToolResultInfo | undefined;
  cwd: string;
}) {
  const [open, setOpen] = createSignal(false);
  const pending = () => !props.result;
  const isError = () => props.result?.is_error ?? false;

  const preview = () => truncate(toolPreview(props.name, props.input, props.cwd), 120);

  const stateClass = () => {
    if (pending()) return 'compact-tool-pending';
    if (isError()) return 'compact-tool-err';
    return 'compact-tool-done';
  };

  // For bash, extract timeout from input for display on collapsed line
  const bashTimeout = () => {
    if (props.name !== 'bash') return '';
    const raw = (props.input as any)?.timeout;
    const t = typeof raw === 'string' ? parseInt(raw, 10) : raw;
    if (typeof t !== 'number' || isNaN(t)) return '';
    return t >= 1000 ? (t / 1000) + 's' : t + 'ms';
  };

  return (
    <div class={`compact-tool ${stateClass()}`}>
      <button class="compact-tool-line" onclick={() => setOpen(!open())}>
        <span class="compact-tool-name">{props.name}</span>
        {props.name === 'bash' && typeof (props.input as any)?.command === 'string'
          ? <span class="compact-tool-preview compact-tool-highlighted" innerHTML={highlightInline(truncate((props.input as any).command, 120), 'bash')} />
          : <span class="compact-tool-preview">{preview()}</span>
        }
        <Show when={bashTimeout()}>
          <span class="compact-tool-timeout">{bashTimeout()}</span>
        </Show>
      </button>
      <Show when={open()}>
        <div class="collapsible-body">
          {/* Bash: highlight command as bash. Read/edit/write: highlight as JSON.
              Tool output: highlight read/edit results by file extension, bash output stays plain. */}
          {props.name === 'bash' && typeof (props.input as any)?.command === 'string'
            ? <div innerHTML={highlight((props.input as any).command, 'bash')} />
            : <div innerHTML={highlight(JSON.stringify(props.input, null, 2), 'json')} />
          }
          <Show when={props.result}>
            {(() => {
              const res = props.result!;
              const text = extractText(res.content);
              const details = res.details;
              
              if (props.name === 'bash' && details && typeof details.exit_code === 'number') {
                return (
                  <div class="bash-result">
                    <div class="bash-header">
                      <span class={`bash-exit-code ${details.exit_code === 0 ? 'success' : 'fail'}`}>
                        exit {details.exit_code}
                      </span>
                    </div>
                    <Show when={details.stdout}>
                      <pre class="bash-stdout">{details.stdout}</pre>
                    </Show>
                    <Show when={details.stderr}>
                      <pre class="bash-stderr">{details.stderr}</pre>
                    </Show>
                  </div>
                );
              }

              if (props.name === 'read' && details && typeof details.content === 'string') {
                const lang = langFromPath(details.path || '');
                return (
                  <div class="read-result">
                    <div class="file-meta">{details.total_lines} lines</div>
                    <div class="file-content" innerHTML={highlight(details.content, lang)} />
                  </div>
                );
              }

              if (props.name === 'edit' && details && typeof details.old_text === 'string') {
                return (
                  <div class="edit-result">
                    <div class="edit-diff">
                      <div class="diff-removed">
                        <span class="diff-sign">-</span>
                        <pre>{details.old_text}</pre>
                      </div>
                      <div class="diff-added">
                        <span class="diff-sign">+</span>
                        <pre>{details.new_text}</pre>
                      </div>
                    </div>
                  </div>
                );
              }

              const errClass = res.is_error ? 'tool-output-err' : 'tool-output-ok';
              // Highlight read/edit/write results by file extension; bash output stays plain
              const path = typeof (props.input as any)?.path === 'string' ? (props.input as any).path : '';
              const lang = (props.name === 'read' || props.name === 'write') ? langFromPath(path) : '';
              if (lang) {
                return <div class={errClass} innerHTML={highlight(text, lang)} />;
              }
              return <pre class={errClass}>{text}</pre>;
            })()}
          </Show>
        </div>
      </Show>
    </div>
  );
}

// --- Content block dispatchers ---

function DebugBlockView(props: { block: ContentBlock }) {
  switch (props.block.type) {
    case 'text':
      return <div class="md-text" innerHTML={marked(props.block.text) as string} />;
    case 'thinking':
      return <ThinkingBlock text={props.block.thinking} />;
    case 'tool_use':
      return <ToolUseBlock name={props.block.name} input={props.block.input} />;
    case 'tool_result':
      return <ToolResultBlock content={props.block.content} isError={props.block.is_error} />;
    case 'image':
      return (
        <div class="content-image">
          <img src={`data:${props.block.mediaType};base64,${props.block.data}`} alt="" />
        </div>
      );
    case 'error':
      return <ErrorBlock message={props.block.message} />;
    default:
      return null;
  }
}

function CompactBlockView(props: {
  block: ContentBlock;
  toolResults: Map<string, ToolResultInfo>;
  cwd: string;
}) {
  switch (props.block.type) {
    case 'text':
      return <div class="md-text" innerHTML={marked(props.block.text) as string} />;
    case 'thinking':
      return <ThinkingBlock text={props.block.thinking} />;
    case 'tool_use':
      return (
        <CompactToolCall
          name={props.block.name}
          input={props.block.input}
          result={props.toolResults.get(props.block.id)}
          cwd={props.cwd}
        />
      );
    // tool_result blocks are not rendered in compact mode --
    // they are merged into the tool_use line above.
    case 'tool_result':
      return null;
    case 'image':
      return (
        <div class="content-image">
          <img src={`data:${props.block.mediaType};base64,${props.block.data}`} alt="" />
        </div>
      );
    case 'error':
      return <ErrorBlock message={props.block.message} />;
    default:
      return null;
  }
}

// --- Main message component ---

export interface MessageViewProps {
  message: Message;
  mode: DisplayMode;
  toolResults: Map<string, ToolResultInfo>;
  cwd: string;
}

export default function MessageView(props: MessageViewProps) {
  const displayRole = (): string => {
    const m = props.message;
    if (m.role === 'user' && m.content.every(b => b.type === 'tool_result')) return 'tool';
    if (m.role === 'assistant') return 'asst';
    return m.role;
  };

  const timestamp = () => {
    const ts = props.message.provenance?.ts;
    if (!ts) return '';
    return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  };

  if (props.message.role === 'system') {
    return <SystemMessage message={props.message} mode={props.mode} />;
  }

  // In compact mode, hide user messages that are purely tool results
  // (they're merged into the assistant's tool_use lines).
  const isToolOnlyUser = () =>
    props.message.role === 'user' &&
    props.message.content.every(b => b.type === 'tool_result');

  const isCompact = () => props.mode === 'compact';

  // Use Show for reactive visibility -- early returns are not reactive in SolidJS.
  return (
    <Show when={!(isCompact() && isToolOnlyUser())}>
      <div class="msg">
        {/* Message header: role + timestamp + optional debug id */}
        <div class="msg-meta">
          <span class={`msg-role role-${displayRole()}`}>{displayRole()}</span>
          <Show when={timestamp()}>
            <span class="msg-ts">{timestamp()}</span>
          </Show>
          <Show when={props.mode === 'debug'}>
            <span class="msg-id">{props.message.id}</span>
          </Show>
        </div>

        {/* Content blocks */}
        <div class="msg-body">
          <For each={props.message.content}>
            {(block) => isCompact()
              ? <CompactBlockView block={block} toolResults={props.toolResults} cwd={props.cwd} />
              : <DebugBlockView block={block} />
            }
          </For>
        </div>

        {/* Usage stats in debug mode for assistant messages */}
        <Show when={props.mode === 'debug' && props.message.provenance?.usage}>
          <UsageStats
            usage={props.message.provenance!.usage!}
            model={props.message.provenance!.model}
          />
        </Show>
      </div>
    </Show>
  );
}
