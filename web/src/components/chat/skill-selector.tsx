/**
 * Skill 选择器占位组件。
 * 旧 SDK 中 skills 相关端点已下线,组件保留以保持 chat-input 调用兼容,
 * 实际选择行为暂时禁用。
 */
import { Sparkles } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';

interface SkillSelectorProps {
  cwd: string | null;
  disabled?: boolean;
  onSelect: (skill: { name: string; path: string }) => void;
}

export function SkillSelector({ disabled, onSelect }: SkillSelectorProps) {
  const { t } = useTranslation();
  return (
    <Button
      size="sm"
      variant="ghost"
      className="h-7 gap-1 rounded-lg px-2 text-xs"
      title={t('Skills')}
      disabled={disabled}
      onClick={() => onSelect({ name: '', path: '' })}
    >
      <Sparkles className="h-3.5 w-3.5" />
    </Button>
  );
}
