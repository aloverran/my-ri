import { SessionSummary, SessionDetail, Message, Usage } from './types';

// API helpers
const apiUrl = (path: string) => `/api${path}`;

async function apiRequest<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(apiUrl(path), {
    method,
    headers: {
      'Content-Type': 'application/json',
    },
    body: body ? JSON.stringify(body) : undefined,
  });

  if (!response.ok) {
    throw new Error(`API request failed: ${response.status} ${response.statusText}`);
  }

  const text = await response.text();
  return text ? JSON.parse(text) : (undefined as unknown as T);
}

// Session endpoints
export function getSessions(): Promise<SessionSummary[]> {
  return apiRequest<SessionSummary[]>('GET', '/sessions');
}

export function getSession(id: string): Promise<SessionDetail> {
  return apiRequest<SessionDetail>('GET', `/sessions/${id}`);
}

export function createSession(name: string, cwd: string): Promise<{ id: string }> {
  return apiRequest<{ id: string }>('POST', '/sessions', { name, cwd });
}

export function deleteSession(id: string): Promise<void> {
  return apiRequest<void>('DELETE', `/sessions/${id}`);
}

export function sendMessage(
  sessionId: string,
  text: string,
  model?: string,
  thinking?: string,
): Promise<void> {
  return apiRequest<void>('POST', `/sessions/${sessionId}/messages`, { text, model, thinking });
}

export function cancelSession(sessionId: string): Promise<void> {
  return apiRequest<void>('POST', `/sessions/${sessionId}/cancel`);
}

// Models
export interface ModelInfo {
  id: string;
  name: string;
  provider: string;
  context_window: number;
}

export function getModels(): Promise<ModelInfo[]> {
  return apiRequest<ModelInfo[]>('GET', '/models');
}

// Settings (server defaults)
export interface SettingsData {
  default_model: string;
  default_thinking: string;
}

export function getSettings(): Promise<SettingsData> {
  return apiRequest<SettingsData>('GET', '/settings');
}

// Auth -- OAuth login for LLM providers.

export interface ProviderAuthInfo {
  id: string;
  name: string;
  authenticated: boolean;
  account?: string;
}

export function getAuthStatus(): Promise<ProviderAuthInfo[]> {
  return apiRequest<ProviderAuthInfo[]>('GET', '/auth/status');
}

export interface AuthLoginResponse {
  method: 'paste_code' | 'local_callback';
  url: string;
}

export function beginLogin(providerId: string): Promise<AuthLoginResponse> {
  return apiRequest<AuthLoginResponse>('POST', '/auth/login', { provider_id: providerId });
}

export function completeLogin(providerId: string, code: string): Promise<void> {
  return apiRequest<void>('POST', '/auth/complete', { provider_id: providerId, code });
}

export interface AuthLoginStatusResponse {
  status: 'awaiting_code' | 'awaiting_callback' | 'complete' | 'failed';
  error: string | null;
}

export function getLoginStatus(providerId: string): Promise<AuthLoginStatusResponse> {
  return apiRequest<AuthLoginStatusResponse>('GET', `/auth/login-status/${providerId}`);
}

// SSE handlers interface -- data shapes match the backend's JSON payloads.
export interface SSEHandlers {
  text_start?: () => void;
  text_delta?: (data: { delta: string }) => void;
  text_end?: () => void;
  thinking_start?: () => void;
  thinking_delta?: (data: { delta: string }) => void;
  thinking_end?: () => void;
  tool_start?: (data: { id: string; name: string }) => void;
  tool_end?: (data: { id: string; output: string; is_error: boolean; details?: Record<string, unknown> }) => void;
  message_complete?: (data: Message) => void;
  usage?: (data: Usage) => void;
  done?: () => void;
  agent_error?: (data: { message: string }) => void;
  resync?: () => void;
  error?: (error: Event) => void;
}

// SSE connection.
// Wires each SSEHandlers key to an EventSource listener. Signal events
// (no payload) call the handler directly; data events JSON.parse the
// payload first. Adding a new event type only requires adding a key to
// SSEHandlers -- the wiring loop picks it up automatically.
export function connectSSE(sessionId: string, handlers: SSEHandlers): EventSource {
  const eventSource = new EventSource(apiUrl(`/sessions/${sessionId}/events`));

  // Signal events carry no data. All other handler keys (except 'error')
  // carry a JSON payload that gets parsed and forwarded.
  const signals = new Set(['text_start', 'text_end', 'thinking_start', 'thinking_end', 'done', 'resync']);

  for (const [name, handler] of Object.entries(handlers)) {
    if (name === 'error' || !handler) continue;
    // Cast: Object.entries loses per-key type correlation. Type safety is
    // enforced at the SSEHandlers interface; this loop is just plumbing.
    const fn = handler as (...args: unknown[]) => void;
    if (signals.has(name)) {
      eventSource.addEventListener(name, () => fn());
    } else {
      eventSource.addEventListener(name, (e) =>
        fn(JSON.parse((e as MessageEvent).data))
      );
    }
  }

  if (handlers.error) eventSource.onerror = handlers.error;
  return eventSource;
}

// -- Log SSE --

export interface LogEntry {
  ts: string;
  level: 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR';
  target: string;
  message: string;
}

/** Connect to the global tracing log SSE stream. */
export function connectLogSSE(onEntry: (entry: LogEntry) => void): EventSource {
  const es = new EventSource(apiUrl('/logs'));
  es.addEventListener('log', (e) => {
    onEntry(JSON.parse((e as MessageEvent).data));
  });
  return es;
}
