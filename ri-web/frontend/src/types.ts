export interface SessionSummary {
  id: string;
  name: string;
  ts: string;
  cwd: string;
  parent?: string;
  message_count: number;
}

export interface SessionDetail {
  id: string;
  name: string;
  ts: string;
  cwd: string;
  parent?: string;
  status: "idle" | "running";
  messages: Message[];
}

// -- Navigation --

/** Where the app is: session list or viewing a specific session. */
export type NavState =
  | { view: 'list' }
  | { view: 'session'; id: string };

export interface Message {
  id: string;
  role: "system" | "user" | "assistant";
  content: ContentBlock[];
  meta?: Record<string, unknown>;
}

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string; sig?: string }
  | { type: "tool_use"; id: string; name: string; input: unknown }
  | { type: "tool_result"; toolUseId: string; content: ContentBlock[]; is_error: boolean; details?: Record<string, unknown> }
  | { type: "image"; mediaType: string; data: string }
  | { type: "error"; message: string };

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  extras?: Record<string, unknown>;
}

export type DisplayMode = 'compact' | 'debug';

export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'k';
  return n.toString();
}

export function relativeTime(ts: string): string {
  const diff = Date.now() - new Date(ts).getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'now';
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d`;
  return new Date(ts).toLocaleDateString();
}