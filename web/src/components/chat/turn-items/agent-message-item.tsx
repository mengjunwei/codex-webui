import type { TurnItem } from '@/types/timeline';

interface Props {
  item: TurnItem;
}

export function AgentMessageItem({ item }: Props) {
  return (
    <div>
      <pre className="m-0 whitespace-pre-wrap font-sans text-sm leading-relaxed wrap-break-word">
        {item.content}
      </pre>
    </div>
  );
}
