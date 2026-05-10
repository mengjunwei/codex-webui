/**
 * Renders a single AI turn as a unified block.
 * Contains all items (reasoning, tool calls, messages) under one avatar.
 */
import { Bot, Loader2 } from 'lucide-react';
import { motion } from 'framer-motion';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import type { TimelineEntry, TurnItem } from '@/types/timeline';
import { ReasoningItem } from './turn-items/reasoning-item';
import { AgentMessageItem } from './turn-items/agent-message-item';
import { ToolCallItem } from './turn-items/tool-call-item';
import { CommandItem } from './turn-items/command-item';

const entryVariants = {
  hidden: { opacity: 0, y: 10 },
  visible: {
    opacity: 1,
    y: 0,
    transition: { type: 'spring' as const, stiffness: 400, damping: 30 },
  },
};

interface Props {
  entry: Extract<TimelineEntry, { kind: 'turn' }>;
}

function renderItem(item: TurnItem) {
  switch (item.type) {
    case 'reasoning':
      return <ReasoningItem key={item.itemId} item={item} />;
    case 'agentMessage':
      return <AgentMessageItem key={item.itemId} item={item} />;
    case 'mcpToolCall':
      return <ToolCallItem key={item.itemId} item={item} />;
    case 'commandExecution':
      return <CommandItem key={item.itemId} item={item} />;
  }
}

export function TurnBlock({ entry }: Props) {
  return (
    <motion.div
      variants={entryVariants}
      initial="hidden"
      animate="visible"
      className="mb-6 flex gap-3"
    >
      <Avatar className="mt-1 h-8 w-8 shrink-0">
        <AvatarFallback className="bg-muted">
          <Bot className="h-4 w-4" />
        </AvatarFallback>
      </Avatar>

      <div className="min-w-0 flex-1 space-y-2">
        {entry.items.map(renderItem)}

        {!entry.completed && entry.items.length === 0 && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Thinking...
          </div>
        )}
      </div>
    </motion.div>
  );
}
