/**
 * Codex app-server config management tab.
 *
 * TODO: codexConfigReadConfigOptions / codexConfigUpdateConfigMutation 等
 *       旧 SDK 调用已下线,待迁移到新 mt-client API。当前仅渲染占位说明。
 */
import { useTranslation } from 'react-i18next';

export function CodexSettings() {
  const { t } = useTranslation();

  return (
    <section className="space-y-4">
      <div className="space-y-1">
        <h2 className="text-sm font-medium text-muted-foreground">
          {t('Codex Configuration')}
        </h2>
        <p className="text-xs text-muted-foreground">
          {t(
            'Manage Codex app-server settings. Changes are saved to user config.toml and hot-reloaded.',
          )}
        </p>
      </div>

      <div className="rounded-lg border border-border bg-card/50 px-4 py-3 text-sm text-muted-foreground">
        {t('Codex config editor is temporarily unavailable.')}
      </div>
    </section>
  );
}
