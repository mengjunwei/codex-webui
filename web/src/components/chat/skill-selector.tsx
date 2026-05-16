/**
 * Skill selector chip in ChatInput bottom bar.
 * Click → Popover with search + skill list from GET /api/skills.
 */
import { useState, useMemo } from 'react';
import { Zap, Loader2 } from 'lucide-react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { skillsListSkillsOptions } from '@/generated/api/@tanstack/react-query.gen';
import { cn } from '@/lib/utils';

export interface SkillSelection {
  name: string;
  path: string;
}

interface Props {
  cwd: string | null;
  disabled?: boolean;
  onSelect: (skill: SkillSelection) => void;
}

export function SkillSelector({ cwd, disabled, onSelect }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');

  const { data, isLoading } = useQuery({
    ...skillsListSkillsOptions({ query: { cwd: cwd ?? '' } }),
    enabled: open && Boolean(cwd),
  });

  // Flatten skills from response (data.data is unknown[] from passthrough DTO)
  interface SkillEntry {
    skills?: Array<{ name: string; description: string; path: string; enabled: boolean }>;
  }
  const skills = useMemo(() => {
    if (!data?.data) return [];
    return (data.data as SkillEntry[]).flatMap((entry) =>
      (entry.skills ?? []).filter((s) => s.enabled),
    );
  }, [data]);

  // Filter by search
  const filtered = useMemo(() => {
    const lowerSearch = search.toLowerCase();
    return skills.filter(
      (s) =>
        s.name.toLowerCase().includes(lowerSearch) ||
        s.description.toLowerCase().includes(lowerSearch),
    );
  }, [skills, search]);

  const handleSelect = (skill: { name: string; path: string }) => {
    onSelect({ name: skill.name, path: skill.path });
    setOpen(false);
    setSearch('');
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          size="sm"
          variant="ghost"
          className="h-7 gap-1.5 rounded-lg px-2.5 text-xs"
          disabled={disabled || !cwd}
          title={t('Add skill')}
        >
          <Zap className="h-3.5 w-3.5" />
          {t('Skill')}
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-64 p-0" align="start" side="top">
        <div className="border-b border-border px-3 py-2">
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('Search skills...')}
            className="w-full bg-transparent text-xs outline-none placeholder:text-muted-foreground"
            autoFocus
          />
        </div>
        <div className="max-h-48 overflow-y-auto py-1">
          {isLoading ? (
            <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              {t('Loading...')}
            </div>
          ) : filtered.length === 0 ? (
            <div className="p-3 text-xs text-muted-foreground">
              {skills.length === 0 ? t('No skills available') : t('No matching skills')}
            </div>
          ) : (
            filtered.map((skill) => (
              <button
                key={skill.path}
                type="button"
                onClick={() => handleSelect(skill)}
                className={cn(
                  'flex w-full flex-col gap-0.5 px-3 py-1.5 text-left transition-colors hover:bg-accent/50',
                )}
              >
                <span className="text-xs font-medium text-foreground">{skill.name}</span>
                {skill.description && (
                  <span className="line-clamp-1 text-[11px] text-muted-foreground">
                    {skill.description}
                  </span>
                )}
              </button>
            ))
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
