/**
 * Apps 标签页 — 当前后端 apps 相关端点已下线,显示占位说明。
 */
import { useTranslation } from 'react-i18next';

export function AppsTab() {
  const { t } = useTranslation();
  return (
    <div className="rounded-lg border border-border bg-card/50 px-4 py-3 text-sm text-muted-foreground">
      {t('Apps integration is temporarily unavailable.')}
    </div>
  );
}
