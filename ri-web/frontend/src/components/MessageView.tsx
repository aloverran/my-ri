import { createSignal, For, Show } from 'solid-js';
import { marked } from 'marked';
import { Message, ContentBlock, DisplayMode } from '../types';

/** Data needed to resolve tool_use -> tool_result in compact mode. */
export interface ToolResultInfo {
  content: ContentBlock[];
  is_error: boolean;
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
        <div class="collapsible-body"><pre>{JSON.stringify(props.input, null, 2)}</pre></div>
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

  return (
    <div class={`compact-tool ${stateClass()}`}>
      <button class="compact-tool-line" onclick={() => setOpen(!open())}>
        <span class="compact-tool-name">{props.name}</span>
        <span class="compact-tool-preview">{preview()}</span>
      </button>
      <Show when={open()}>
        <div class="collapsible-body">
          <pre>{JSON.stringify(props.input, null, 2)}</pre>
          <Show when={props.result}>
            <pre class={props.result?.is_error ? 'tool-output-err' : 'tool-output-ok'}>
              {extractText(props.result!.content)}
            </pre>
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
      </div>
    </Show>
  );
}
