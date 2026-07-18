/**
 * Account 设置页 — 显示当前用户信息 + 个人 API key 管理 + 登出按钮。
 * 多租户模式下,账号信息通过 /api/mt/auth/* 获取。
 */
import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { User, Loader2, Key, Eye, EyeOff } from 'lucide-react';
import { clearApiToken, clearRefreshToken } from '@/auth-token';
import { showSnackbar } from '@/stores/snackbar-store';
import { useNavigate } from '@tanstack/react-router';
import { mtFetch } from '@/lib/mt-client';

interface UserInfo {
  id: string;
  email: string;
  display_name?: string;
}

interface ApiKeyResp {
  id: string;
  provider: string;
  key_hint: string;
  is_active: boolean;
  created_at: number;
}

export function AccountSettings() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [user, setUser] = useState<UserInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [apiKey, setApiKey] = useState('');
  const [showKey, setShowKey] = useState(false);
  const [provider, setProvider] = useState('openai');

  useEffect(() => {
    try {
      const token = sessionStorage.getItem('codex.webui.jwt');
      if (token) {
        const payload = JSON.parse(atob(token.split('.')[1]));
        setUser({ id: payload.sub, email: payload.email || 'unknown' });
      }
    } catch {
      setUser(null);
    } finally {
      setLoading(false);
    }
  }, []);

  // 获取用户个人 API key 列表
  const keysQuery = useQuery({
    queryKey: ['user', 'api-keys'],
    queryFn: () => mtFetch<ApiKeyResp[]>('/user/api-key'),
  });

  // 设置/轮换个人 API key
  const setKeyMutation = useMutation({
    mutationFn: (body: { key: string; provider?: string }) =>
      mtFetch<ApiKeyResp>('/user/api-key', 'POST', body),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['user', 'api-keys'] });
      setApiKey('');
      showSnackbar(t('API key saved'), 'success');
    },
    onError: (err: Error) => showSnackbar(err.message, 'error'),
  });

  const handleLogout = () => {
    clearApiToken();
    clearRefreshToken();
    void navigate({ to: '/', search: {} });
    showSnackbar(t('Logged out'), 'success');
  };

  const handleSaveKey = () => {
    if (!apiKey.trim()) return;
    setKeyMutation.mutate({ key: apiKey.trim(), provider });
  };

  if (loading) return <div className="flex items-center gap-2 text-sm text-muted-foreground py-4"><Loader2 className="h-4 w-4 animate-spin" />{t('Loading...')}</div>;

  const activeKey = keysQuery.data?.find((k) => k.is_active);

  return (
    <div className="space-y-6 py-4">
      {/* 用户信息 */}
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

      {/* 个人 API Key 管理 */}
      <div className="space-y-3 border-t pt-4">
        <div className="flex items-center gap-2">
          <Key className="h-4 w-4" />
          <span className="text-sm font-medium">{t('Personal API Key')}</span>
        </div>
        <p className="text-xs text-muted-foreground">
          {t('Set your personal OpenAI API key for your personal workspace. Leave empty if using a local proxy.')}
        </p>

        {activeKey && (
          <div className="flex items-center gap-2 text-sm">
            <Badge variant="default">{t('Active')}</Badge>
            <span className="text-muted-foreground">{activeKey.key_hint}</span>
            <span className="text-xs text-muted-foreground">({activeKey.provider})</span>
          </div>
        )}

        <div className="flex gap-2">
          <div className="relative flex-1">
            <Input
              type={showKey ? 'text' : 'password'}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              placeholder={t('Enter API key (sk-...) or leave empty for local proxy')}
              className="pr-10"
            />
            <button
              type="button"
              onClick={() => setShowKey(!showKey)}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            >
              {showKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
            </button>
          </div>
          <select
            value={provider}
            onChange={(e) => setProvider(e.target.value)}
            className="rounded-md border bg-background px-3 py-2 text-sm"
          >
            <option value="openai">OpenAI</option>
            <option value="anthropic">Anthropic</option>
            <option value="custom">Custom</option>
          </select>
          <Button
            onClick={handleSaveKey}
            disabled={setKeyMutation.isPending}
          >
            {setKeyMutation.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : t('Save')}
          </Button>
        </div>

        {keysQuery.isLoading && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />{t('Loading keys...')}
          </div>
        )}
      </div>

      {/* 登出 */}
      <div className="border-t pt-4">
        <Button variant="destructive" onClick={handleLogout} className="w-full">
          {t('Sign out')}
        </Button>
      </div>
    </div>
  );
}
