/** DTOs for updating Codex config values via config/batchWrite. */
import { ApiProperty } from '@nestjs/swagger';
import { APPROVAL_POLICY_VALUES, jsonValueSchema } from './v2/openapi.schema';

export const SANDBOX_MODE_VALUES = [
  'read-only',
  'workspace-write',
  'danger-full-access',
] as const;

export const CODEX_CONFIG_EDITABLE_KEYS = [
  'profile',
  'model',
  'review_model',
  'model_provider',
  'model_context_window',
  'model_auto_compact_token_limit',
  'instructions',
  'developer_instructions',
  'compact_prompt',
  'model_reasoning_effort',
  'model_reasoning_summary',
  'model_verbosity',
  'web_search',
  'service_tier',
] as const;

const JSON_OBJECT_SCHEMA = {
  type: 'object',
  additionalProperties: true,
} as const;

/** Request body for updating the approval policy. */
export class UpdateApprovalPolicyDto {
  @ApiProperty({ enum: APPROVAL_POLICY_VALUES })
  approvalPolicy!: (typeof APPROVAL_POLICY_VALUES)[number];
}

/** Request body for updating the sandbox mode. */
export class UpdateSandboxModeDto {
  @ApiProperty({ enum: SANDBOX_MODE_VALUES })
  sandboxMode!: (typeof SANDBOX_MODE_VALUES)[number];
}

/** Single curated config edit accepted by PATCH /api/codex/config. */
export class ConfigEditDto {
  @ApiProperty({ enum: CODEX_CONFIG_EDITABLE_KEYS })
  keyPath!: (typeof CODEX_CONFIG_EDITABLE_KEYS)[number];

  @ApiProperty(jsonValueSchema(true))
  value!: unknown;
}

/** Request body for curated Codex config updates. */
export class UpdateCodexConfigDto {
  @ApiProperty({ type: () => [ConfigEditDto] })
  edits!: ConfigEditDto[];
}

/** Full Codex config/read response after JSON-safe conversion and redaction. */
export class CodexConfigResponseDto {
  @ApiProperty(JSON_OBJECT_SCHEMA)
  config!: Record<string, unknown>;

  @ApiProperty(JSON_OBJECT_SCHEMA)
  origins!: Record<string, unknown>;
}

/** Raw user config.toml content returned for Monaco editing. */
export class RawConfigResponseDto {
  @ApiProperty()
  filePath!: string;

  @ApiProperty()
  content!: string;
}

/** Response returned after replacing raw user config.toml content. */
export class RawConfigWriteResponseDto {
  @ApiProperty()
  filePath!: string;
}

/** Request body for replacing raw user config.toml content. */
export class UpdateRawConfigDto {
  @ApiProperty()
  content!: string;
}
