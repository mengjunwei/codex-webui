/** A single item within an AI turn. */
export interface TurnItem {
  type:
    | 'reasoning'
    | 'agentMessage'
    | 'mcpToolCall'
    | 'commandExecution'
    | 'fileChange';
  itemId: string;
  content: string;
  completed: boolean;
  toolName?: string;
  toolServer?: string;
  toolArgs?: string;
  /** File path for fileChange items. */
  filePath?: string;
  /** Pure diff content from changes[0].diff (fileChange only). */
  fileDiff?: string;
  /** Shell command for commandExecution items. */
  command?: string;
  /** Exit code for commandExecution items. */
  exitCode?: number;
}

/** A user message, system message, or a full AI turn. */
export type TimelineEntry =
  | { kind: 'user'; content: string }
  | { kind: 'system'; content: string; severity?: 'info' | 'warning' | 'error' }
  | {
      kind: 'turn';
      turnId: string;
      items: TurnItem[];
      completed: boolean;
      /** Turn-level unified diff across all file changes. */
      diff?: string;
    };
