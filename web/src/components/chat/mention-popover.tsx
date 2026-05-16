/**
 * Popover triggered by @ in ChatInput for file/directory search.
 * Supports path-based navigation: typing `/` drills into directories.
 * Click directory = navigate into; click file = select as mention.
 */
import { useEffect, useRef } from 'react';
import { File, Folder, Loader2, Paperclip } from 'lucide-react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { filesReadTreeOptions } from '@/generated/api/@tanstack/react-query.gen';
import { unescapeMentionPath } from '@/lib/mention-utils';
import { cn } from '@/lib/utils';

export interface MentionResult {
  name: string;
  path: string;
  type: 'file' | 'directory';
}

interface Props {
  open: boolean;
  /** Full text after @, may include path segments (e.g., "src/components/ch"). */
  query: string;
  /** Thread cwd root. */
  cwd: string | null;
  selectedIndex: number;
  /** Ref filled with filtered items so parent can read current selection on Enter. */
  filteredRef: React.RefObject<MentionResult[]>;
  /** Called when a file is selected (or directory is pinned). */
  onSelect: (result: MentionResult) => void;
  /** Called when user navigates into a directory (click or Enter on dir). */
  onNavigate: (dirPath: string) => void;
  /** Called when user clicks a breadcrumb segment to go back to that level. */
  onNavigateUp: (relativePath: string) => void;
}

/**
 * Parses the query into a browse directory (relative to cwd) and a filter string.
 * e.g., "src/components/ch" → { browseRelative: "src/components", filter: "ch" }
 * e.g., "src/" → { browseRelative: "src", filter: "" }
 * e.g., "main" → { browseRelative: "", filter: "main" }
 */
function parseQuery(query: string) {
  const lastSlash = query.lastIndexOf('/');
  if (lastSlash < 0) {
    return { browseRelative: '', browsePath: '', filterText: unescapeMentionPath(query) };
  }
  const browseRelative = query.slice(0, lastSlash);
  return {
    browseRelative,
    browsePath: unescapeMentionPath(browseRelative),
    filterText: unescapeMentionPath(query.slice(lastSlash + 1)),
  };
}

export function MentionPopover({ open, query, cwd, selectedIndex, filteredRef, onSelect, onNavigate, onNavigateUp }: Props) {
  const { t } = useTranslation();
  const listRef = useRef<HTMLDivElement>(null);

  const { browseRelative, browsePath, filterText } = parseQuery(query);

  // Resolve the directory to fetch: cwd + unescaped browse path
  const browseDir = !cwd ? '' : !browsePath ? cwd : `${cwd}/${browsePath}`;

  const { data: entries, isLoading } = useQuery({
    ...filesReadTreeOptions({ query: { root: browseDir } }),
    enabled: open && Boolean(browseDir),
  });

  // Filter entries by the unescaped text after last /
  const lowerFilter = filterText.toLowerCase();
  const filtered: MentionResult[] = entries
    ? entries
        .filter((e) => e.name.toLowerCase().includes(lowerFilter))
        .slice(0, 20)
        .map((e) => ({ name: e.name, path: e.path, type: e.type as 'file' | 'directory' }))
    : [];

  // Sync filtered items to parent ref
  useEffect(() => {
    filteredRef.current = filtered;
  }, [filtered, filteredRef]);

  // Scroll selected item into view
  useEffect(() => {
    if (!listRef.current) return;
    const item = listRef.current.children[selectedIndex] as HTMLElement | undefined;
    item?.scrollIntoView({ block: 'nearest' });
  }, [selectedIndex]);

  if (!open) return null;

  // Breadcrumb showing current navigation path
  const pathSegments = browseRelative ? browseRelative.split('/') : [];

  return (
    <div className="absolute bottom-full z-50 mb-1 w-72 rounded-lg border border-border bg-popover shadow-lg">
      {/* Clickable breadcrumb for navigation */}
      <div className="flex items-center gap-0.5 border-b border-border/60 px-3 py-1.5 text-[11px] text-muted-foreground">
        <button
          type="button"
          onClick={() => onNavigateUp('')}
          className="rounded px-0.5 hover:bg-accent hover:text-foreground"
        >
          /
        </button>
        {pathSegments.map((seg, i) => {
          const segPath = pathSegments.slice(0, i + 1).join('/');
          return (
            <span key={i} className="flex items-center gap-0.5">
              <button
                type="button"
                onClick={() => onNavigateUp(segPath)}
                className="rounded px-0.5 hover:bg-accent hover:text-foreground"
              >
                {unescapeMentionPath(seg)}
              </button>
              {i < pathSegments.length - 1 && <span className="opacity-40">/</span>}
            </span>
          );
        })}
      </div>

      {isLoading ? (
        <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t('Loading...')}
        </div>
      ) : filtered.length === 0 ? (
        <div className="p-3 text-xs text-muted-foreground">
          {t('No matching files')}
        </div>
      ) : (
        <div ref={listRef} className="max-h-52 overflow-y-auto py-1">
          {filtered.map((entry, i) => (
            <div
              key={entry.path}
              className={cn(
                'group flex w-full items-center gap-2 px-3 py-1.5 text-xs transition-colors',
                i === selectedIndex ? 'bg-accent text-accent-foreground' : 'text-foreground hover:bg-accent/50',
              )}
            >
              {entry.type === 'directory' ? (
                /* Directory: click name area = navigate in; 📎 at right = mention dir */
                <>
                  <button
                    type="button"
                    className="flex min-w-0 flex-1 items-center gap-2 text-left"
                    onClick={() => onNavigate(entry.path)}
                  >
                    <Folder className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                    <span className="min-w-0 truncate">{entry.name}</span>
                  </button>
                  {/* Attach directory as mention */}
                  <button
                    type="button"
                    title={t('Attach to chat')}
                    onClick={() => onSelect(entry)}
                    className="shrink-0 rounded p-0.5 text-muted-foreground opacity-0 transition-opacity hover:bg-background hover:text-foreground group-hover:opacity-100"
                  >
                    <Paperclip className="h-3 w-3" />
                  </button>
                </>
              ) : (
                /* File: click = select */
                <button
                  type="button"
                  className="flex min-w-0 flex-1 items-center gap-2 text-left"
                  onClick={() => onSelect(entry)}
                >
                  <File className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  <span className="min-w-0 truncate">{entry.name}</span>
                </button>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
