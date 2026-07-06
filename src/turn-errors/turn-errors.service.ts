/** Persists final turn errors from Codex app-server notifications for hydration after refresh. */
import { Inject, Injectable, Logger, OnModuleInit } from '@nestjs/common';
import { eq } from 'drizzle-orm';
import { CodexProcessManager } from '../codex/codex-process-manager.service';
import type { ServerNotification } from '../codex/codex-schema';
import { DRIZZLE_DB, type AppDatabase } from '../database/database.constants';
import { turnErrors, type TurnErrorRow } from '../database/schema';
import type {
  ThreadTurnErrorsResponseDto,
  PersistedTurnErrorDto,
} from './dto/turn-error.dto';

@Injectable()
export class TurnErrorsService implements OnModuleInit {
  private readonly logger = new Logger(TurnErrorsService.name);

  constructor(
    private readonly codexManager: CodexProcessManager,
    @Inject(DRIZZLE_DB) private readonly db: AppDatabase,
  ) {}

  onModuleInit(): void {
    this.codexManager.addListener(
      'notification',
      (notification: ServerNotification) => {
        if (notification.method === 'error') {
          this.handleErrorNotification(notification.params);
        } else if (notification.method === 'turn/completed') {
          this.handleTurnCompleted(notification.params);
        }
      },
    );
  }

  /** Reads all persisted turn errors for a thread. */
  readThreadErrors(threadId: string): ThreadTurnErrorsResponseDto {
    const rows = this.db
      .select()
      .from(turnErrors)
      .where(eq(turnErrors.threadId, threadId))
      .orderBy(turnErrors.createdAt)
      .all();

    return {
      threadId,
      errors: rows.map((row) => this.toDto(row)),
    };
  }

  /**
   * Handles `error` notifications. Only persists final errors (willRetry=false)
   * that have both threadId and turnId.
   */
  private handleErrorNotification(params: Record<string, unknown>): void {
    if (params.willRetry) return;

    const threadId = params.threadId as string | undefined;
    const turnId = params.turnId as string | undefined;
    if (!threadId || !turnId) return;

    const error = params.error as { message?: string } | undefined;
    const message = error?.message ?? 'Unknown error';

    this.upsert(threadId, turnId, message);
  }

  /** Handles `turn/completed` notifications with status='failed'. */
  private handleTurnCompleted(params: Record<string, unknown>): void {
    const threadId = params.threadId as string | undefined;
    const turn = params.turn as
      | {
          id?: string;
          status?: string;
          error?: { message?: string } | null;
        }
      | undefined;

    if (!threadId || !turn?.id) return;
    if (turn.status !== 'failed' || !turn.error?.message) return;

    this.upsert(threadId, turn.id, turn.error.message);
  }

  /** Upserts a turn error — last error for a given turn wins. */
  private upsert(threadId: string, turnId: string, message: string): void {
    try {
      const now = Date.now();
      this.db
        .insert(turnErrors)
        .values({ threadId, turnId, message, createdAt: now })
        .onConflictDoUpdate({
          target: [turnErrors.threadId, turnErrors.turnId],
          set: { message, createdAt: now },
        })
        .run();
    } catch (err) {
      this.logger.warn(
        `Failed to persist turn error for thread=${threadId} turn=${turnId}: ${(err as Error).message}`,
      );
    }
  }

  private toDto(row: TurnErrorRow): PersistedTurnErrorDto {
    return {
      turnId: row.turnId,
      message: row.message,
      createdAt: row.createdAt,
    };
  }
}
