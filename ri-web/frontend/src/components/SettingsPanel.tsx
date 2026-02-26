import { createSignal, createResource, For, Show, onCleanup } from 'solid-js';
import {
  getAuthStatus, beginLogin, completeLogin, logout, getLoginStatus,
  ProviderAuthInfo, AuthLoginResponse,
} from '../api';

// -- Login flow state machine --
// Each provider can be in one of these states. The transitions are:
//   idle -> paste_code (waiting for user to paste) -> idle
//   idle -> local_callback (waiting for browser redirect) -> idle
// On completion, we refetch auth status to update the UI.
type LoginState =
  | { phase: 'idle' }
  | { phase: 'paste_code'; url: string }
  | { phase: 'local_callback'; url: string }
  | { phase: 'completing' }
  | { phase: 'done' }
  | { phase: 'error'; message: string };

interface SettingsPanelProps {
  onClose: () => void;
}

export default function SettingsPanel(props: SettingsPanelProps) {
  const [providers, { refetch }] = createResource(getAuthStatus);

  // Per-provider login state, keyed by provider id.
  const [loginStates, setLoginStates] = createSignal<Record<string, LoginState>>({});

  // Track active poll intervals for cleanup when the panel unmounts.
  // onCleanup only works in the component's synchronous setup scope,
  // not inside async callbacks, so we track them here instead.
  const activePolls: ReturnType<typeof setInterval>[] = [];
  onCleanup(() => activePolls.forEach(id => clearInterval(id)));

  const loginState = (id: string): LoginState => loginStates()[id] ?? { phase: 'idle' };

  const setProviderState = (id: string, state: LoginState) => {
    setLoginStates(prev => ({ ...prev, [id]: state }));
  };

  // Start login for a provider. Calls begin_login, then branches on method.
  const startLogin = async (providerId: string) => {
    setProviderState(providerId, { phase: 'completing' });
    try {
      const resp: AuthLoginResponse = await beginLogin(providerId);

      if (resp.method === 'paste_code') {
        // Open auth URL in new tab. User will see the code there.
        window.open(resp.url, '_blank');
        setProviderState(providerId, { phase: 'paste_code', url: resp.url });
      } else {
        // LocalCallback: open URL, then poll for completion.
        window.open(resp.url, '_blank');
        setProviderState(providerId, { phase: 'local_callback', url: resp.url });
        pollLoginStatus(providerId);
      }
    } catch (e) {
      setProviderState(providerId, { phase: 'error', message: String(e) });
    }
  };

  // Poll login status for LocalCallback flows until complete or failed.
  const pollLoginStatus = (providerId: string) => {
    const interval = setInterval(async () => {
      try {
        const status = await getLoginStatus(providerId);
        if (status.status === 'complete') {
          clearInterval(interval);
          setProviderState(providerId, { phase: 'done' });
          refetch();
        } else if (status.status === 'failed') {
          clearInterval(interval);
          setProviderState(providerId, {
            phase: 'error',
            message: status.error ?? 'Login failed',
          });
        }
        // awaiting_callback: keep polling
      } catch {
        clearInterval(interval);
        setProviderState(providerId, { phase: 'error', message: 'Lost connection' });
      }
    }, 1000);

    activePolls.push(interval);
  };

  // Submit the pasted code for a PasteCode flow.
  const submitCode = async (providerId: string, code: string) => {
    if (!code.trim()) return;
    setProviderState(providerId, { phase: 'completing' });
    try {
      await completeLogin(providerId, code.trim());
      setProviderState(providerId, { phase: 'done' });
      refetch();
    } catch (e) {
      setProviderState(providerId, { phase: 'error', message: String(e) });
    }
  };

  // Logout from a provider: delete stored credentials.
  const performLogout = async (providerId: string) => {
    setProviderState(providerId, { phase: 'completing' });
    try {
      await logout(providerId);
      setProviderState(providerId, { phase: 'idle' });
      refetch();
    } catch (e) {
      setProviderState(providerId, { phase: 'error', message: String(e) });
    }
  };

  return (
    <div class="settings-panel">
      {/* Header */}
      <div class="settings-panel-header">
        <span class="settings-panel-title">Settings</span>
        <button class="settings-panel-close" onclick={props.onClose}>{'\u00D7'}</button>
      </div>

      {/* Provider auth section */}
      <div class="settings-section">
        <div class="settings-section-label">Providers</div>
        <Show when={providers.loading}><div class="loading">Loading...</div></Show>
        <Show when={providers.error}><div class="error-text">Failed to load</div></Show>
        <For each={providers()}>
          {(provider) => (
            <ProviderRow
              provider={provider}
              state={loginState(provider.id)}
              onLogin={() => startLogin(provider.id)}
              onLogout={() => performLogout(provider.id)}
              onSubmitCode={(code) => submitCode(provider.id, code)}
              onRetry={() => setProviderState(provider.id, { phase: 'idle' })}
            />
          )}
        </For>
      </div>
    </div>
  );
}

// -- Provider row --
// Shows auth status and login controls for a single provider.

interface ProviderRowProps {
  provider: ProviderAuthInfo;
  state: LoginState;
  onLogin: () => void;
  onLogout: () => void;
  onSubmitCode: (code: string) => void;
  onRetry: () => void;
}

function ProviderRow(props: ProviderRowProps) {
  const [code, setCode] = createSignal('');

  const handleCodeSubmit = (e: Event) => {
    e.preventDefault();
    props.onSubmitCode(code());
    setCode('');
  };

  return (
    <div class="provider-row">
      {/* Top line: indicator + name + action */}
      <div class="provider-row-top">
        <span class={`provider-dot ${props.provider.authenticated ? 'auth-ok' : 'auth-none'}`} />
        <span class="provider-name">{props.provider.name}</span>
        <span class="provider-id">{props.provider.id}</span>
        <Show when={props.provider.account}>
          <span class="provider-account">{props.provider.account}</span>
        </Show>

        <Show when={props.state.phase === 'idle' && !props.provider.authenticated}>
          <button class="provider-login-btn" onclick={props.onLogin}>Login</button>
        </Show>
        <Show when={props.state.phase === 'idle' && props.provider.authenticated}>
          <button class="provider-login-btn" onclick={props.onLogin}>Re-login</button>
          <Show when={props.provider.can_logout}
            fallback={<span class="provider-env-tag">env var</span>}
          >
            <button class="provider-logout-btn" onclick={props.onLogout}>Logout</button>
          </Show>
        </Show>
        <Show when={props.state.phase === 'completing'}>
          <span class="provider-status">connecting...</span>
        </Show>
        <Show when={props.state.phase === 'local_callback'}>
          <span class="provider-status waiting">waiting for browser...</span>
        </Show>
        <Show when={props.state.phase === 'done'}>
          <span class="provider-status ok">logged in</span>
        </Show>
      </div>

      {/* Paste code input (Anthropic flow) */}
      <Show when={props.state.phase === 'paste_code'}>
        <form class="paste-code-form" onSubmit={handleCodeSubmit}>
          <input
            type="text"
            class="paste-code-input"
            placeholder="Paste authorization code..."
            value={code()}
            onInput={(e) => setCode(e.currentTarget.value)}
            autofocus
          />
          <button type="submit" class="primary" disabled={!code().trim()}>Submit</button>
        </form>
      </Show>

      {/* Error state */}
      <Show when={props.state.phase === 'error'}>
        <div class="provider-error">
          <span class="error-text">{(props.state as { phase: 'error'; message: string }).message}</span>
          <button onclick={props.onRetry}>Retry</button>
        </div>
      </Show>
    </div>
  );
}
