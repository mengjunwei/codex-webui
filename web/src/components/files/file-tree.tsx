/**
 * Recursive file tree component with lazy-loading directories.
 * Root directory comes from the active thread's cwd.
 */
import {
  ChevronRight,
  File,
  Folder,
  FolderOpen,
  Loader2,
} from 'lucide-react';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useFilesStore } from '@/stores/files-store';
import { cn } from '@/lib/utils';

export function FileTree() {
  const rootDir = useFilesStore((s) => s.rootDir);
  const tree = useFilesStore((s) => s.tree);
  const selectedFile = useFilesStore((s) => s.selectedFile);

  if (!rootDir) {
    return (
      <div className="px-3 py-8 text-center text-xs text-muted-foreground">
        No workspace directory
      </div>
    );
  }

  return (
    <ScrollArea className="min-h-0 flex-1">
      <div className="py-1">
        <DirectoryContents
          dirPath={rootDir}
          tree={tree}
          depth={0}
          selectedFile={selectedFile}
        />
      </div>
    </ScrollArea>
  );
}

interface DirectoryContentsProps {
  dirPath: string;
  tree: Map<string, { name: string; path: string; type: string; expanded?: boolean }[]>;
  depth: number;
  selectedFile: string | null;
}

/** Renders the contents of a loaded directory. */
function DirectoryContents({ dirPath, tree, depth, selectedFile }: DirectoryContentsProps) {
  const toggleDirectory = useFilesStore((s) => s.toggleDirectory);
  const selectFile = useFilesStore((s) => s.selectFile);

  const children = tree.get(dirPath);
  if (!children) {
    return (
      <div
        className="flex items-center gap-1 py-1 text-xs text-muted-foreground"
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
      >
        <Loader2 className="h-3 w-3 animate-spin" />
        Loading...
      </div>
    );
  }

  return (
    <>
      {children.map((entry) => {
        if (entry.type === 'directory') {
          const isExpanded = entry.expanded ?? false;
          return (
            <div key={entry.path}>
              <TreeRow
                depth={depth}
                icon="directory"
                name={entry.name}
                expanded={isExpanded}
                selected={false}
                onClick={() => toggleDirectory(entry.path)}
              />
              {isExpanded && (
                <DirectoryContents
                  dirPath={entry.path}
                  tree={tree}
                  depth={depth + 1}
                  selectedFile={selectedFile}
                />
              )}
            </div>
          );
        }
        return (
          <TreeRow
            key={entry.path}
            depth={depth}
            icon="file"
            name={entry.name}
            selected={entry.path === selectedFile}
            onClick={() => void selectFile(entry.path)}
          />
        );
      })}
    </>
  );
}

interface TreeRowProps {
  depth: number;
  icon: 'file' | 'directory';
  name: string;
  expanded?: boolean;
  selected: boolean;
  onClick: () => void;
}

function TreeRow({ depth, icon, name, expanded, selected, onClick }: TreeRowProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        'flex w-full items-center gap-1 py-0.5 text-left text-xs transition-colors hover:bg-accent/50',
        selected && 'bg-accent text-accent-foreground',
      )}
      style={{ paddingLeft: `${depth * 16 + 8}px` }}
    >
      {icon === 'directory' ? (
        <>
          <ChevronRight
            className={cn(
              'h-3 w-3 shrink-0 transition-transform',
              expanded && 'rotate-90',
            )}
          />
          {expanded ? (
            <FolderOpen className="h-3.5 w-3.5 shrink-0 text-blue-400" />
          ) : (
            <Folder className="h-3.5 w-3.5 shrink-0 text-blue-400" />
          )}
        </>
      ) : (
        <>
          <span className="w-3" />
          <File className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
        </>
      )}
      <span className="truncate">{name}</span>
    </button>
  );
}
