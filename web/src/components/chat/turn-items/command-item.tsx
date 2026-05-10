import { Loader2 } from 'lucide-react';
import type { TurnItem } from '@/types/timeline';

interface Props {
  item: TurnItem;
}

export function CommandItem({ item }: Props) {
  return (
    <div className="overflow-hidden rounded-lg border border-border/50 bg-muted/40 font-mono">
      <div className="border-b border-border/50 px-3 py-1.5 text-xs text-muted-foreground">
        Terminal
        {!item.completed && (
          <Loader2 className="ml-1.5 inline h-3 w-3 animate-spin" />
        )}
      </div>
      <pre className="m-0 overflow-x-auto p-3 text-xs leading-relaxed">
        {item.content}
      </pre>
    </div>
  );
}
