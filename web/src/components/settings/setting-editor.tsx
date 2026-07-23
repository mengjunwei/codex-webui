/** Reusable editor for a single runtime setting with Save/Reset actions. */
import { RotateCcw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  settingLabel,
  sourceLabel,
  sourceVariant,
  formatSettingValue,
  type RuntimeSetting,
} from './setting-helpers';

interface Props {
  setting: RuntimeSetting;
  draft: string;
  disabled: boolean;
  onDraftChange: (key: string, value: string) => void;
  onSave: (setting: RuntimeSetting) => void;
  onReset: (key: string) => void;
  /**
   * 只读模式:非平台管理员查看全局运行时配置时启用。
   * 隐藏 Save/Reset 写控件、Input disabled,并显示权限提示。
   * (后端 PATCH /api/settings 已限制平台管理员,前端隐藏仅为 UX,避免 403。)
   */
  readOnly?: boolean;
}

export function SettingEditor({
  setting,
  draft,
  disabled,
  onDraftChange,
  onSave,
  onReset,
  readOnly = false,
}: Props) {
  const { t } = useTranslation();
  const isDbOverride = setting.source === 'db';
  const inputDisabled = disabled || readOnly;

  return (
    <div className="space-y-3 rounded-lg border border-border bg-card/50 px-4 py-3">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-sm font-medium">
              {settingLabel(setting.key)}
            </h3>
            <Badge variant={sourceVariant(setting.source)}>
              {t(sourceLabel(setting.source))}
            </Badge>
          </div>
          <p className="text-xs text-muted-foreground">
            {t(setting.description)}
          </p>
          <p className="text-xs text-muted-foreground">
            {t('Default')}: {formatSettingValue(setting.defaultValue)}
          </p>
        </div>
      </div>

      <div className="flex flex-wrap items-end gap-2">
        {setting.type === 'number' ? (
          (() => {
            const constraints = (setting.constraints ?? {}) as {
              min?: number;
              max?: number;
              integer?: boolean;
            };
            return (
              <Input
                type="number"
                value={draft}
                min={constraints.min}
                max={constraints.max}
                step={constraints.integer ? 1 : undefined}
                disabled={inputDisabled}
                onChange={(e) => onDraftChange(setting.key, e.target.value)}
                className="h-8 w-40"
              />
            );
          })()
        ) : (
          <Input
            value={draft}
            disabled={inputDisabled}
            onChange={(e) => onDraftChange(setting.key, e.target.value)}
            className="h-8 w-64"
          />
        )}

        {!readOnly && (
          <>
            <Button
              size="sm"
              className="h-8"
              disabled={disabled}
              onClick={() => onSave(setting)}
            >
              {t('Save')}
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8"
              disabled={disabled || !isDbOverride}
              onClick={() => onReset(setting.key)}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              {t('Reset')}
            </Button>
          </>
        )}
      </div>

      {readOnly && (
        <p className="text-xs text-muted-foreground">
          {t('只读:仅平台管理员可修改全局配置。')}
        </p>
      )}
    </div>
  );
}
