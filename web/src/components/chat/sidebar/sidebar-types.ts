/** Shared types and pure helpers for the thread sidebar. */
import type { ThreadDto } from '@/lib/mt-client';
export type { ThreadDto } from '@/lib/mt-client';

// SidebarView type is defined in layout-store.ts as SidebarViewState.
// Re-export for backward compatibility with child components.
export type { SidebarViewState as SidebarView } from '@/stores/layout-store';

export type ConfirmAction =
  | { type: 'archive'; thread: ThreadDto }
  | { type: 'compact'; thread: ThreadDto }
  | { type: 'delete'; thread: ThreadDto }
  | null;

export interface WorkspaceGroup {
  /** 分组键:'__personal__'(个人 workspace) 或 team_id。 */
  key: string;
  /** 显示名:个人 workspace / team 名称。 */
  label: string;
  workspace_type: 'personal' | 'team';
  /** active 会话(默认展开)。 */
  active: ThreadDto[];
  /** 归档会话(折叠子区)。 */
  archived: ThreadDto[];
}

/** Display label for a thread: title → id prefix. */
export function threadLabel(thread: ThreadDto): string {
  return thread.title?.trim() || thread.id.slice(0, 8);
}

/** Extract the last path segment from a cwd for display. */
export function workspaceLabel(cwd: string): string {
  const parts = cwd.split('/').filter(Boolean);
  return parts.at(-1) ?? cwd;
}

/** 按会话归属分组:个人 workspace 单独一组,每个 team 一组;组内再分 active / archived。
 *  个人 workspace 置顶,团队按传入顺序(已按 last_activity_at 倒序)。 */
export function groupByWorkspace(
  threads: ThreadDto[],
  teams: Array<{ id: string; name: string }>,
): WorkspaceGroup[] {
  const map = new Map<string, WorkspaceGroup>();
  for (const th of threads) {
    const isPersonal = th.workspace_type === 'personal';
    const key = isPersonal ? '__personal__' : th.team_id;
    if (!map.has(key)) {
      const label = isPersonal
        ? '个人 workspace'
        : (teams.find((t) => t.id === key)?.name ?? '团队');
      map.set(key, {
        key,
        label,
        workspace_type: isPersonal ? 'personal' : 'team',
        active: [],
        archived: [],
      });
    }
    const g = map.get(key)!;
    (th.status === 'archived' ? g.archived : g.active).push(th);
  }
  const groups = Array.from(map.values());
  // 个人 workspace 置顶,团队保持插入顺序。
  groups.sort((a, b) => {
    const av = a.workspace_type === 'personal' ? 0 : 1;
    const bv = b.workspace_type === 'personal' ? 0 : 1;
    return av - bv;
  });
  return groups;
}
