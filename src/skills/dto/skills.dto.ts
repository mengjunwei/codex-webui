import { ApiProperty } from '@nestjs/swagger';
import { jsonValueSchema } from '../../codex/dto/v2/openapi.schema';

/** Raw skills/list response passthrough from Codex app-server. */
export class SkillsListResponseDto {
  @ApiProperty({
    type: 'array',
    items: jsonValueSchema(false) as Record<string, unknown>,
  })
  data!: unknown[];
}
