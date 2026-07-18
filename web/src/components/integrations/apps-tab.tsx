/**
 * Apps 标签页 — 后端 apps/models 端点已下线,显示功能说明。
 * 模型配置通过 /api/codex/status 读取。
 */
import { useTranslation } from 'react-i18next';
import { Bot } from 'lucide-react';

export function AppsTab() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-center justify-center gap-3 py-12 text-center text-muted-foreground">
      <Bot className="h-8 w-8 opacity-50" />
      <p className="text-sm">{t('Model configuration is managed via the server config file (config.toml).')}</p>
      <p className="text-xs">{t('Check /api/codex/status for the current model provider status.')}</p>
    </div>
  );
}
