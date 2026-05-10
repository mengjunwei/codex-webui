/** A single item within an AI turn. */
export interface TurnItem {
  type: 'reasoning' | 'agentMessage' | 'mcpToolCall' | 'commandExecution';
  itemId: string;
  content: string;
  completed: boolean;
  toolName?: string;
  toolServer?: string;
  toolArgs?: string;
}

/** A user message, system message, or a full AI turn. */
export type TimelineEntry =
  | { kind: 'user'; content: string }
  | { kind: 'system'; content: string }
  | { kind: 'turn'; turnId: string; items: TurnItem[]; completed: boolean };
