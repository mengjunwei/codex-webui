/**
 * Login route — multi-tenant email + password auth.
 * Reads ?redirect= search param to return to the original page after login.
 */
import { useCallback } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { LoginPage } from '@/components/login';
import { SnackbarContainer } from '@/components/snackbar/snackbar-container';
import { setApiToken, setRefreshToken, clearApiToken, clearRefreshToken } from '@/auth-token';
import { authApi } from '@/lib/mt-client';
import { resetSocket } from '@/socket';

export function LoginRoute() {
  const navigate = useNavigate();
  const { redirect } = useSearch({ from: '/login' });

  const handleLogin = useCallback(async (email: string, password: string): Promise<boolean> => {
    try {
      const data = await authApi.login({ email, password }) as { accessToken: string; refreshToken: string };
      setApiToken(data.accessToken);
      setRefreshToken(data.refreshToken);
      resetSocket();
      void navigate({ to: redirect });
      return true;
    } catch {
      clearApiToken();
      clearRefreshToken();
      return false;
    }
  }, [navigate, redirect]);

  return (
    <>
      <LoginPage onLogin={handleLogin} />
      <SnackbarContainer />
    </>
  );
}
