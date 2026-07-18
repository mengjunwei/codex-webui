/** Shared types and pure helpers for the thread sidebar.
 *  TODO: ThreadDto 来自旧 OpenAPI SDK,已下线。当前从 mt-client 返回的 thread 形状是 any,
 *        这里用本地最小结构 + any,待后端补全 OpenAPI 注解后再恢复强类型。
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type ThreadDto = any;

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

/** Display label for a thread: name → preview → truncated id. */
export function threadLabel(thread: ThreadDto): string {
  const t = thread as { name?: string; preview?: string; id: string };
  return t.name?.trim() || t.preview || t.id.slice(0, 8);
}

/** Extract the last path segment from a cwd for display. */
export function workspaceLabel(cwd: string): string {
  const parts = cwd.split('/').filter(Boolean);
  return parts.at(-1) ?? cwd;
}

/** Group threads by cwd, preserving insertion order. */
export function groupByWorkspace(threads: ThreadDto[]): WorkspaceGroup[] {
  const groups = new Map<string, ThreadDto[]>();
  for (const thread of threads) {
    const t = thread as { cwd: string };
    const group = groups.get(t.cwd) ?? [];
    group.push(thread);
    groups.set(t.cwd, group);
  }
  return Array.from(groups.entries()).map(([cwd, groupThreads]) => ({
    cwd,
    threads: groupThreads,
  }));
}
