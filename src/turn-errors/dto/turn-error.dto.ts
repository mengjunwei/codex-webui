/** Swagger DTOs for persisted turn error hydration. */
import { ApiProperty } from '@nestjs/swagger';

/** A single persisted turn error (named to avoid collision with Codex TurnErrorDto). */
export class PersistedTurnErrorDto {
  @ApiProperty()
  turnId!: string;

  @ApiProperty()
  message!: string;

  @ApiProperty()
  createdAt!: number;
}

/** Turn error query response for hydrating the frontend store. */
export class ThreadTurnErrorsResponseDto {
  @ApiProperty()
  threadId!: string;

  @ApiProperty({ type: () => [PersistedTurnErrorDto] })
  errors!: PersistedTurnErrorDto[];
}
