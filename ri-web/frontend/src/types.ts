export interface SessionSummary {
  id: string;
  name: string;
  ts: string;
  cwd: string;
  message_count: number;
}

export interface SessionDetail {
  id: string;
  name: string;
  ts: string;
  cwd: string;
  status: "idle" | "running";
  messages: Message[];
}

export interface Message {
  id: string;
  role: "system" | "user" | "assistant";
  content: ContentBlock[];
  provenance?: Provenance;
  meta?: Record<string, unknown>;
}

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string; sig?: string }
  | { type: "tool_use"; id: string; name: string; input: unknown }
  | { type: "tool_result"; toolUseId: string; content: ContentBlock[]; is_error: boolean; details?: Record<string, unknown> }
  | { type: "image"; mediaType: string; data: string }
  | { type: "error"; message: string };

export interface Provenance {
  input: string[];
  model: string;
  ts: string;
  usage?: Usage;
}

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