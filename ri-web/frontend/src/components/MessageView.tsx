import { createSignal, For, Show } from 'solid-js';
import { marked } from 'marked';
import { Message, ContentBlock } from '../types';

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + '...';
}

function toolPreview(name: string, input: unknown): string {
  if (!input || typeof input !== 'object') return JSON.stringify(input).slice(0, 80);
  const obj = input as Record<string, unknown>;
  switch (name) {
    case 'bash': return typeof obj.command === 'string' ? obj.command : '';
    case 'read': return typeof obj.path === 'string' ? obj.path : '';
    case 'write': return typeof obj.path === 'string' ? obj.path : '';
    case 'edit': return typeof obj.path === 'string' ? obj.path : '';
    default: return JSON.stringify(input).slice(0, 80);
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

// --- Collapsible sub-components ---

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
  const preview = () => truncate(toolPreview(props.name, props.input), 80);
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

function SystemMessage(props: { message: Message }) {
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

// --- Content block dispatcher ---

function BlockView(props: { block: ContentBlock }) {
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

// --- Main message component ---

interface MessageViewProps {
  message: Message;
}

export default function MessageView(props: MessageViewProps) {
  // Tool-result-only user messages get labeled "tool"
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
    return <SystemMessage message={props.message} />;
  }

  return (
    <div class="msg">
      <div class="msg-meta">
        <span class={`msg-role role-${displayRole()}`}>{displayRole()}</span>
        <Show when={timestamp()}>
          <span class="msg-ts">{timestamp()}</span>
        </Show>
      </div>
      <div class="msg-body">
        <For each={props.message.content}>
          {(block) => <BlockView block={block} />}
        </For>
      </div>
    </div>
  );
}
