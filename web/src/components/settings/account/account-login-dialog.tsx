/**
 * Account 登录对话框 — 旧端点已下线,当前为邮箱+密码登录。
 * 多租户模式下,账号管理通过 /api/mt/auth/* 端点。
 */
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { showSnackbar } from '@/stores/snackbar-store';
import { useUserStore } from '@/stores/user-store';

interface Props {
  open: boolean;
  onClose: () => void;
  onSuccess?: () => void;
}

export function AccountLoginDialog({ open, onClose }: Props) {
  const { t } = useTranslation();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [loading, setLoading] = useState(false);

  const handleLogin = async () => {
    if (!email.trim() || !password.trim()) return;
    setLoading(true);
    try {
      const res = await fetch('/api/mt/auth/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email: email.trim(), password: password.trim() }),
      });
      if (!res.ok) throw new Error('Login failed');
      const data = await res.json() as { accessToken: string; refreshToken: string };
      sessionStorage.setItem('codex.webui.jwt', data.accessToken);
      sessionStorage.setItem('codex.webui.refreshToken', data.refreshToken);
      // 登录成功后补拉 /me,刷新 useIsPlatformAdmin / usePermission 驱动的 UI 显隐
      // (AuthenticatedLayout 仅在挂载且 me 为空时拉一次,这里不补则权限态恒为旧值)。
      void useUserStore.getState().loadMe();
      showSnackbar(t('Login successful'), 'success');
      onClose();
    } catch {
      showSnackbar(t('Login failed'), 'error');
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('Account Login')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <Input
            type="email"
            placeholder={t('Email')}
            value={email}
            onChange={(e) => setEmail(e.target.value)}
          />
          <Input
            type="password"
            placeholder={t('Password')}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <Button onClick={() => void handleLogin()} disabled={loading || !email.trim() || !password.trim()} className="w-full">
            {loading ? 'Loading...' : t('Sign in')}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
