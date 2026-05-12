/** Overview view: pinned archive group + workspace groups with collapse. */
import { Archive, ChevronDown, ChevronRight } from 'lucide-react';
import { AnimatePresence, motion } from 'framer-motion';
import { useTranslation } from 'react-i18next';
import { Skeleton } from '@/components/ui/skeleton';
import type { ThreadDto } from '@/generated/api';
import type { WorkspaceGroup } from './sidebar-types';
import { workspaceLabel } from './sidebar-types';

const collapseVariants = {
  open: { height: 'auto', opacity: 1 },
  closed: { height: 0, opacity: 0 },
} as const;

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
  archivedThreads: ThreadDto[];
  workspaceGroups: WorkspaceGroup[];
  collapsedGroups: Set<string>;
  isLoading: boolean;
  onToggleCollapse: (key: string) => void;
  onOpenArchivedDetail: () => void;
  onOpenWorkspaceDetail: (cwd: string) => void;
  renderThreadRow: (thread: ThreadDto, archived: boolean) => React.ReactNode;
}

export function WorkspaceOverview({
  archivedThreads,
  workspaceGroups,
  collapsedGroups,
  isLoading,
  onToggleCollapse,
  onOpenArchivedDetail,
  onOpenWorkspaceDetail,
  renderThreadRow,
}: Props) {
  const { t } = useTranslation();
  const archivedCollapsed = collapsedGroups.has('__archived__');

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

  return (
    <div className="space-y-3 pb-2">
      {/* Pinned archive group */}
      <section>
        <div className="mb-0.5 flex items-center gap-1 px-2">
          <button
            type="button"
            className="flex min-w-0 flex-1 cursor-pointer items-center gap-1.5 text-sm font-medium text-foreground/80 hover:text-foreground"
            onClick={() => onToggleCollapse('__archived__')}
          >
            {archivedCollapsed
              ? <ChevronRight className="h-3.5 w-3.5 shrink-0 transition-transform" />
              : <ChevronDown className="h-3.5 w-3.5 shrink-0 transition-transform" />}
            <Archive className="h-3.5 w-3.5 shrink-0" />
            {t('Archived')}
          </button>
          <button
            type="button"
            className="shrink-0 cursor-pointer whitespace-nowrap text-xs text-muted-foreground hover:text-foreground hover:underline"
            onClick={onOpenArchivedDetail}
          >
            {t('View more')}
          </button>
        </div>
        <AnimatePresence initial={false}>
          {!archivedCollapsed && (
            <motion.div
              key="archived-content"
              initial="closed"
              animate="open"
              exit="closed"
              variants={collapseVariants}
              transition={{ duration: 0.15, ease: 'easeInOut' }}
              className="overflow-hidden"
            >
              <div className="space-y-0.5">
                {archivedThreads.map((thread) => renderThreadRow(thread, true))}
                {archivedThreads.length === 0 && (
                  <p className="py-2 pl-5 text-xs text-muted-foreground">{t('No archived threads')}</p>
                )}
              </div>
            </motion.div>
          )}
        </AnimatePresence>
      </section>

      {/* Workspace groups */}
      {workspaceGroups.map((group) => {
        const collapsed = collapsedGroups.has(group.cwd);
        return (
          <section key={group.cwd}>
            <div className="mb-0.5 flex items-center gap-1 px-2" title={group.cwd}>
              <button
                type="button"
                className="flex min-w-0 flex-1 cursor-pointer items-center gap-1.5 text-sm font-medium text-foreground/80 hover:text-foreground"
                onClick={() => onToggleCollapse(group.cwd)}
              >
                {collapsed
                  ? <ChevronRight className="h-3.5 w-3.5 shrink-0 transition-transform" />
                  : <ChevronDown className="h-3.5 w-3.5 shrink-0 transition-transform" />}
                <span className="truncate">{workspaceLabel(group.cwd)}</span>
              </button>
              {group.threads.length >= 5 && (
                <button
                  type="button"
                  className="shrink-0 cursor-pointer whitespace-nowrap text-xs text-muted-foreground hover:text-foreground hover:underline"
                  onClick={() => onOpenWorkspaceDetail(group.cwd)}
                >
                  {t('View more')}
                </button>
              )}
            </div>
            <AnimatePresence initial={false}>
              {!collapsed && (
                <motion.div
                  key={`ws-${group.cwd}`}
                  initial="closed"
                  animate="open"
                  exit="closed"
                  variants={collapseVariants}
                  transition={{ duration: 0.15, ease: 'easeInOut' }}
                  className="overflow-hidden"
                >
                  <div className="space-y-0.5">
                    {group.threads.slice(0, 5).map((thread) => renderThreadRow(thread, false))}
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
          </section>
        );
      })}

      {workspaceGroups.length === 0 && archivedThreads.length === 0 && (
        <p className="px-2 py-8 text-center text-xs text-muted-foreground">
          {t('No threads yet')}
        </p>
      )}
    </div>
  );
}
