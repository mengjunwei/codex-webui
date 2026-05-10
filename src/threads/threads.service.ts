/**
 * Handles thread and turn operations by delegating to Codex app-server.
 */
import { Injectable } from '@nestjs/common';
import { CodexService } from '../codex/codex.service';
import type { v2 } from '../codex/codex-schema';

@Injectable()
export class ThreadsService {
  constructor(private readonly codex: CodexService) {}

  /**
   * Creates a new thread (conversation).
   *
   * @param params - Thread start parameters (model, cwd, approvalPolicy, etc.)
   * @returns The created thread with resolved settings
   */
  async startThread(
    params: v2.ThreadStartParams,
  ): Promise<v2.ThreadStartResponse> {
    return this.codex.request<v2.ThreadStartResponse>('thread/start', params);
  }

  /**
   * Lists threads with optional filtering and pagination.
   *
   * @param params - List parameters (cursor, limit, archived, searchTerm, etc.)
   * @returns Paginated thread list
   */
  async listThreads(
    params: v2.ThreadListParams,
  ): Promise<v2.ThreadListResponse> {
    return this.codex.request<v2.ThreadListResponse>('thread/list', params);
  }

  /**
   * Reads a single thread by ID.
   *
   * @param threadId - The thread identifier
   * @param includeTurns - Whether to include turn history
   * @returns The thread data
   */
  async readThread(
    threadId: string,
    includeTurns = false,
  ): Promise<v2.ThreadReadResponse> {
    return this.codex.request<v2.ThreadReadResponse>('thread/read', {
      threadId,
      includeTurns,
    });
  }

  /**
   * Resumes a persisted thread and subscribes to its events.
   *
   * @param threadId - The thread identifier
   * @returns The resumed thread with resolved settings
   */
  async resumeThread(threadId: string): Promise<v2.ThreadResumeResponse> {
    return this.codex.request<v2.ThreadResumeResponse>('thread/resume', {
      threadId,
      persistExtendedHistory: true,
    });
  }

  /**
   * Starts a new turn (user message + agent response cycle).
   *
   * @param params - Turn start parameters (threadId, input, model overrides, etc.)
   * @returns The created turn
   */
  async startTurn(params: v2.TurnStartParams): Promise<v2.TurnStartResponse> {
    return this.codex.request<v2.TurnStartResponse>('turn/start', params);
  }

  /**
   * Interrupts an in-progress turn.
   *
   * @param threadId - The thread identifier
   * @param turnId - The turn to interrupt
   */
  async interruptTurn(threadId: string, turnId: string): Promise<void> {
    await this.codex.request('turn/interrupt', { threadId, turnId });
  }
}
