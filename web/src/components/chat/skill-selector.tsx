/**
 * Skill 选择器 — 后端 skills 端点已下线,显示当前状态提示。
 * 后续可通过 /api/mt/* 补全。
 */
import { useTranslation } from 'react-i18next';

export function SkillSelector() {
  const { t } = useTranslation();
  return (
    <div className="text-xs text-muted-foreground px-2 py-1">
      {t('Skills are managed by the server configuration.')}
    </div>
  );
}
