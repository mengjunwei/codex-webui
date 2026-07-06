/** Shared types and pure helpers for the thread sidebar. */
import type { ThreadDto } from '@/generated/api';

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
  return thread.name?.trim() || thread.preview || thread.id.slice(0, 8);
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
    const group = groups.get(thread.cwd) ?? [];
    group.push(thread);
    groups.set(thread.cwd, group);
  }
  return Array.from(groups.entries()).map(([cwd, groupThreads]) => ({
    cwd,
    threads: groupThreads,
  }));
}
