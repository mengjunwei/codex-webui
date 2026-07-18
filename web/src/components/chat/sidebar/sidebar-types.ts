/** Shared types and pure helpers for the thread sidebar. */
import type { ThreadDto } from '@/lib/mt-client';
export type { ThreadDto } from '@/lib/mt-client';

// SidebarView type is defined in layout-store.ts as SidebarViewState.
// Re-export for backward compatibility with child components.
export type { SidebarViewState as SidebarView } from '@/stores/layout-store';

export type ConfirmAction =
  | { type: 'archive'; thread: ThreadDto }
  | { type: 'compact'; thread: ThreadDto }
  | null;

export interface WorkspaceGroup {
  cwd: string;
  threads: ThreadDto[];
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

/** Group threads by cwd, preserving insertion order.
 *  后端 ThreadDto 没有 cwd 字段,按 status 分组即可。 */
export function groupByWorkspace(threads: ThreadDto[]): WorkspaceGroup[] {
  // 按 status 分组（active / archived）
  const groups = new Map<string, ThreadDto[]>();
  for (const thread of threads) {
    const key = thread.status || 'active';
    const group = groups.get(key) ?? [];
    group.push(thread);
    groups.set(key, group);
  }
  return Array.from(groups.entries()).map(([cwd, groupThreads]) => ({
    cwd,
    threads: groupThreads,
  }));
}
