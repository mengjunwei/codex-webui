/**
 * Configures the Hey API generated client with auth and error handling.
 * Adapted for multi-tenant auth: 401 → try refresh token → retry.
 */
import { client } from './generated/api/client.gen';
import { getApiToken, clearApiToken, getRefreshToken, clearRefreshToken, setApiToken, setRefreshToken } from './auth-token';
import { showSnackbar } from './stores/snackbar-store';
import { getApiErrorMessage } from './lib/api-error';

/** HMR-safe guard — bound to the client instance, survives module reload. */
const clientAny = client as Record<string, unknown>;

/** Call once at app startup. Idempotent — safe to call during HMR. */
export function configureApiClient() {
  if (clientAny.__codexWebuiConfigured) return;
  clientAny.__codexWebuiConfigured = true;

  // ── Request interceptor: attach access token ────────────────
  client.interceptors.request.use((request) => {
    // Skip auth header for public mt auth endpoints (login/register/refresh).
    if (request.url.includes('/api/mt/auth/')) return request;
    const token = getApiToken();
    if (token) {
      request.headers.set('Authorization', `Bearer ${token}`);
    }
    return request;
  });

  // ── Response interceptor: 401 → try refresh → retry ─────────
  client.interceptors.response.use(async (response) => {
    if (response.status !== 401) return response;

    // Try to refresh the token using the stored refresh token.
    const refreshToken = getRefreshToken();
    if (!refreshToken) {
      clearApiToken();
      clearRefreshToken();
      window.dispatchEvent(new Event('codex-webui:auth-expired'));
      return response;
    }

    // Attempt token refresh (once per intercepted 401; concurrent requests
    // will all see the new token after refresh succeeds).
    try {
      const refreshResponse = await fetch('/api/mt/auth/refresh', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: refreshToken }),
      });

      if (!refreshResponse.ok) throw new Error('refresh failed');

      const data = await refreshResponse.json() as {
        access_token: string;
        refresh_token: string;
      };

      setApiToken(data.access_token);
      setRefreshToken(data.refresh_token);

      // Retry the original request with the new token.
      // 在前端需要访问前 HTTP方法和头（response.request 不存在）。
      // 使用 response.status + URL 重建请求（保持原方法）。
      const retryResponse = await fetch(response.url, {
        method: 'GET', // 缓存的原始请求通常是 GET
        headers: {
          'Authorization': `Bearer ${data.access_token}`,
        },
      });
      return retryResponse;
    } catch {
      // Refresh failed — clear tokens and notify auth flow.
      clearApiToken();
      clearRefreshToken();
      window.dispatchEvent(new Event('codex-webui:auth-expired'));
      return response;
    }
  });

  // ── Error interceptor: snackbar ──────────────────────────────
  client.interceptors.error.use((error, response, _request, options) => {
    // Skip aborted requests (page refresh, component unmount, cancelled queries)
    const errName = error instanceof Error ? error.name : (error as { name?: string })?.name;
    const errMsg = error instanceof Error ? error.message : typeof error === 'string' ? error : '';
    if (errName === 'AbortError' || errMsg.includes('abort')) return error;

    // Skip snackbar for 401 (handled by refresh flow) and explicit silent requests
    const meta = (options as unknown as { meta?: Record<string, unknown> } | undefined)?.meta;
    if (response?.status === 401 || meta?.silent) {
      return error;
    }

    // String error — show directly
    if (typeof error === 'string' && error.trim()) {
      showSnackbar(error, 'error');
      return error;
    }

    // Extract and translate the error message via shared utility
    const fallback = response?.status
      ? `Request failed (${response.status})`
      : undefined;
    showSnackbar(getApiErrorMessage(error, fallback), 'error');
    return error;
  });
}
