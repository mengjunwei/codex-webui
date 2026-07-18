/**
 * Account 设置页 — 显示当前用户信息 + 登出按钮。
 * 多租户模式下,账号信息通过 /api/mt/auth/* 获取。
 */
import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { User, Loader2 } from 'lucide-react';
import { clearApiToken, clearRefreshToken } from '@/auth-token';
import { showSnackbar } from '@/stores/snackbar-store';
import { useNavigate } from '@tanstack/react-router';

interface UserInfo {
  id: string;
  email: string;
  display_name?: string;
}

export function AccountSettings() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [user, setUser] = useState<UserInfo | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    try {
      const token = sessionStorage.getItem('codex.webui.jwt');
      if (token) {
        // 解析 JWT payload（不验证签名）
        const payload = JSON.parse(atob(token.split('.')[1]));
        setUser({ id: payload.sub, email: payload.email || 'unknown' });
      }
    } catch {
      setUser(null);
    } finally {
      setLoading(false);
    }
  }, []);

  const handleLogout = () => {
    clearApiToken();
    clearRefreshToken();
    void navigate({ to: '/login' });
    showSnackbar(t('Logged out'), 'success');
  };

  if (loading) return <div className="flex items-center gap-2 text-sm text-muted-foreground py-4"><Loader2 className="h-4 w-4 animate-spin" />{t('Loading...')}</div>;

  return (
    <div className="space-y-4 py-4">
      <div className="flex items-center gap-3">
        <div className="flex h-10 w-10 items-center justify-center rounded-full bg-muted">
          <User className="h-5 w-5" />
        </div>
        <div>
          <div className="text-sm font-medium">{user?.display_name || user?.email || t('Unknown')}</div>
          <div className="text-xs text-muted-foreground">{user?.email || ''}</div>
        </div>
        <Badge variant="secondary" className="ml-auto">{t('Active')}</Badge>
      </div>
      <Button variant="destructive" onClick={handleLogout} className="w-full">
        {t('Sign out')}
      </Button>
    </div>
  );
}
