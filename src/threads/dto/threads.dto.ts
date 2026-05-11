import { ApiProperty, ApiPropertyOptional } from '@nestjs/swagger';
import { approvalPolicySchema } from '../../codex/dto/v2';

/** Request body for creating a Codex thread. */
export class CreateThreadDto {
  @ApiPropertyOptional()
  model?: string;

  @ApiPropertyOptional()
  cwd?: string;

  @ApiPropertyOptional(approvalPolicySchema(true))
  approvalPolicy?: unknown;
}

/** Text-only turn input supported by the current WebUI. */
export class TextTurnInputDto {
  @ApiProperty({ enum: ['text'] })
  type!: 'text';

  @ApiProperty()
  text!: string;
}

/** Request body for starting a new turn. */
export class StartTurnDto {
  @ApiProperty({ type: () => [TextTurnInputDto], minItems: 1 })
  input!: TextTurnInputDto[];
}

export {
  CODEX_V2_EXTRA_MODELS,
  ThreadListResponseDto,
  ThreadReadResponseDto,
  ThreadResumeResponseDto,
  ThreadStartResponseDto,
  TurnStartResponseDto,
} from '../../codex/dto/v2';
