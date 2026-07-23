/** Overview view:按 workspace 分组(个人/各 team),每组 active 默认展开 + archived 折叠子区。 */
import { Archive, ChevronDown, ChevronRight, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Skeleton } from '@/components/ui/skeleton';
import type { ThreadDto, WorkspaceGroup } from './sidebar-types';

function ThreadSkeleton() {
  return (
    <div className="space-y-1.5 pl-3">
      {Array.from({ length: 3 }, (_, i) => (
        <div key={i} className="flex items-center gap-1.5 px-2 py-1.5">
          <Skeleton className="h-3 w-3 shrink-0 rounded" />
          <Skeleton className="h-3 w-24 rounded" />
        </div>
      ))}
    </div>
  );
}

interface Props {
  workspaceGroups: WorkspaceGroup[];
  collapsedGroups: Set<string>;
  isLoading: boolean;
  onToggleCollapse: (key: string) => void;
  onCreateInWorkspace: (group: WorkspaceGroup) => void;
  renderThreadRow: (thread: ThreadDto, archived: boolean) => React.ReactNode;
}

export function WorkspaceOverview({
  workspaceGroups,
  collapsedGroups,
  isLoading,
  onToggleCollapse,
  onCreateInWorkspace,
  renderThreadRow,
}: Props) {
  const { t } = useTranslation();

  if (isLoading) {
    return (
      <div className="space-y-4 pb-2 pt-1">
        {Array.from({ length: 2 }, (_, i) => (
          <div key={i} className="space-y-1.5">
            <div className="flex items-center gap-1.5 px-2">
              <Skeleton className="h-3.5 w-3.5 rounded" />
              <Skeleton className="h-4 w-20 rounded" />
            </div>
            <ThreadSkeleton />
          </div>
        ))}
      </div>
    );
  }

  if (workspaceGroups.length === 0) {
    return (
      <p className="px-2 py-8 text-center text-xs text-muted-foreground">
        {t('No threads yet')}
      </p>
    );
  }

  return (
    <div className="space-y-3 pb-2">
      {workspaceGroups.map((group) => {
        const collapsed = collapsedGroups.has(group.key);
        const archivedKey = `__archived__:${group.key}`;
        const archivedCollapsed = collapsedGroups.has(archivedKey);
        return (
          <section key={group.key}>
            <div className="mb-0.5 flex items-center gap-1 px-2">
              <button
                type="button"
                className="flex min-w-0 flex-1 cursor-pointer items-center gap-1.5 text-sm font-medium text-foreground/80 hover:text-foreground"
                onClick={() => onToggleCollapse(group.key)}
              >
                {collapsed
                  ? <ChevronRight className="h-3.5 w-3.5 shrink-0 transition-transform" />
                  : <ChevronDown className="h-3.5 w-3.5 shrink-0 transition-transform" />}
                <span className="truncate">{group.label}</span>
              </button>
              <button
                type="button"
                className="shrink-0 cursor-pointer rounded p-0.5 text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                onClick={() => onCreateInWorkspace(group)}
                aria-label={t('New thread in this workspace')}
                title={t('New thread in this workspace')}
              >
                <Plus className="h-3.5 w-3.5" />
              </button>
            </div>

            {!collapsed && (
              <div className="space-y-0.5">
                {group.active.map((thread) => renderThreadRow(thread, false))}
                {group.active.length === 0 && group.archived.length === 0 && (
                  <p className="py-1 pl-5 text-xs text-muted-foreground">{t('No threads')}</p>
                )}

                {/* 归档折叠子区 */}
                {group.archived.length > 0 && (
                  <div className="mt-0.5">
                    <button
                      type="button"
                      className="flex w-full cursor-pointer items-center gap-1.5 pl-3 py-1 text-xs text-muted-foreground hover:text-foreground"
                      onClick={() => onToggleCollapse(archivedKey)}
                    >
                      {archivedCollapsed
                        ? <ChevronRight className="h-3 w-3 shrink-0" />
                        : <ChevronDown className="h-3 w-3 shrink-0" />}
                      <Archive className="h-3 w-3 shrink-0" />
                      {t('Archived')} ({group.archived.length})
                    </button>
                    {!archivedCollapsed && (
                      <div className="space-y-0.5">
                        {group.archived.map((thread) => renderThreadRow(thread, true))}
                      </div>
                    )}
                  </div>
                )}
              </div>
            )}
          </section>
        );
      })}
    </div>
  );
}
