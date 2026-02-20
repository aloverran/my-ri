import { SessionSummary, SessionDetail } from './types';

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

export function sendMessage(sessionId: string, text: string): Promise<void> {
  return apiRequest<void>('POST', `/sessions/${sessionId}/messages`, { text });
}

export function cancelSession(sessionId: string): Promise<void> {
  return apiRequest<void>('POST', `/sessions/${sessionId}/cancel`);
}

export interface SettingsData {
  thinking: string;
}

export function getSettings(): Promise<SettingsData> {
  return apiRequest<SettingsData>('GET', '/settings');
}

export function updateSettings(settings: Partial<SettingsData>): Promise<SettingsData> {
  return apiRequest<SettingsData>('PUT', '/settings', settings);
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
  tool_end?: (data: { id: string; output: string; is_error: boolean }) => void;
  message_complete?: (data: any) => void;
  usage?: (data: any) => void;
  done?: () => void;
  error?: (error: any) => void;
  resync?: () => void;
}

// SSE connection
export function connectSSE(sessionId: string, handlers: SSEHandlers): EventSource {
  const eventSource = new EventSource(apiUrl(`/sessions/${sessionId}/events`));

  eventSource.addEventListener('text_start', () => {
    handlers.text_start?.();
  });

  eventSource.addEventListener('text_delta', (event) => {
    const data = JSON.parse(event.data);
    handlers.text_delta?.(data);
  });

  eventSource.addEventListener('text_end', () => {
    handlers.text_end?.();
  });

  eventSource.addEventListener('thinking_start', () => {
    handlers.thinking_start?.();
  });

  eventSource.addEventListener('thinking_delta', (event) => {
    const data = JSON.parse(event.data);
    handlers.thinking_delta?.(data);
  });

  eventSource.addEventListener('thinking_end', () => {
    handlers.thinking_end?.();
  });

  eventSource.addEventListener('tool_start', (event) => {
    const data = JSON.parse(event.data);
    handlers.tool_start?.(data);
  });

  eventSource.addEventListener('tool_end', (event) => {
    const data = JSON.parse(event.data);
    handlers.tool_end?.(data);
  });

  eventSource.addEventListener('message_complete', (event) => {
    const data = JSON.parse(event.data);
    handlers.message_complete?.(data);
  });

  eventSource.addEventListener('usage', (event) => {
    const data = JSON.parse(event.data);
    handlers.usage?.(data);
  });

  eventSource.addEventListener('done', () => {
    handlers.done?.();
  });

  eventSource.addEventListener('resync', () => {
    handlers.resync?.();
  });

  eventSource.onerror = (error) => {
    handlers.error?.(error);
  };

  return eventSource;
}