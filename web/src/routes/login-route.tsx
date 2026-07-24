/** 多租户登录路由：密码、Token 登录和注册。 */
import { useCallback } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { LoginPage } from '@/components/login';
import { SnackbarContainer } from '@/components/snackbar/snackbar-container';
import { setApiToken, setRefreshToken, clearApiToken, clearRefreshToken } from '@/auth-token';
import { authApi } from '@/lib/mt-client';
import { resetSocket } from '@/socket';
import { useUserStore } from '@/stores/user-store';

export function LoginRoute() {
  const navigate = useNavigate();
  const { redirect } = useSearch({ from: '/login' });
  const finish = useCallback((data: { accessToken: string; refreshToken: string }) => {
    setApiToken(data.accessToken); setRefreshToken(data.refreshToken); resetSocket(); void useUserStore.getState().loadMe(); void navigate({ to: redirect });
  }, [navigate, redirect]);
  const handleLogin = useCallback(async (identifier: string, password: string) => {
    try { finish(await authApi.login({ identifier, password })); return true; }
    catch { clearApiToken(); clearRefreshToken(); useUserStore.getState().clearMe(); return false; }
  }, [finish]);
  const handleTokenLogin = useCallback(async (token: string) => {
    try { finish(await authApi.loginWithToken(token)); return true; }
    catch { clearApiToken(); clearRefreshToken(); return false; }
  }, [finish]);
  const handleRegister = useCallback(async (username: string, email: string, password: string) => {
    try { finish(await authApi.register({ username, email, password })); return true; }
    catch { return false; }
  }, [finish]);
  return <><LoginPage onLogin={handleLogin} onTokenLogin={handleTokenLogin} onRegister={handleRegister} /><SnackbarContainer /></>;
}
