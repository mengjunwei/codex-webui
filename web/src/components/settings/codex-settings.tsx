/**
 * Codex 设置页 — 模型配置通过 /api/codex/status 只读查看。
 * 写操作(updateConfig/updateRawConfig)已下线,需重启后端改 config.toml。
 */
import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Loader2, Settings } from 'lucide-react';
import { status as fetchStatus } from '@/generated/api';

interface CodexStatus {
  appServer: { ok: boolean; connected: boolean; initialized: boolean };
  config: { ok: boolean; data?: { approvalPolicy?: string; sandboxMode?: string; model?: string } };
  provider: { ok: boolean; id?: string; name?: string };
}

export function CodexSettings() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<CodexStatus | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    (async () => {
      try {
        const res = await fetchStatus();
        setStatus(res as unknown as CodexStatus);
      } catch {
        setStatus(null);
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  if (loading) return <div className="flex items-center gap-2 text-sm text-muted-foreground py-4"><Loader2 className="h-4 w-4 animate-spin" />{t('Loading...')}</div>;
  if (!status) return <div className="text-sm text-muted-foreground py-4">{t('Unable to load Codex status')}</div>;

  return (
    <div className="space-y-4 py-4">
      <div className="flex items-center gap-2">
        <Settings className="h-4 w-4" />
        <span className="text-sm font-medium">{t('Codex Status')}</span>
        <Badge variant={status.appServer?.ok ? 'default' : 'destructive'}>
          {status.appServer?.ok ? t('Ready') : t('Not ready')}
        </Badge>
      </div>
      {status.config?.data?.model && (
        <div className="text-sm text-muted-foreground">
          {t('Model')}: <span className="font-medium text-foreground">{status.config.data.model}</span>
        </div>
      )}
      {status.config?.data?.approvalPolicy && (
        <div className="text-sm text-muted-foreground">
          {t('Approval policy')}: <span className="font-medium text-foreground">{String(status.config.data.approvalPolicy)}</span>
        </div>
      )}
      {status.config?.data?.sandboxMode && (
        <div className="text-sm text-muted-foreground">
          {t('Sandbox mode')}: <span className="font-medium text-foreground">{String(status.config.data.sandboxMode)}</span>
        </div>
      )}
      {status.provider?.name && (
        <div className="text-sm text-muted-foreground">
          {t('Provider')}: <span className="font-medium text-foreground">{status.provider.name}</span>
        </div>
      )}
      <p className="text-xs text-muted-foreground pt-2 border-t">
        {t('To change settings, edit the server config.toml file and restart the backend.')}
      </p>
    </div>
  );
}
