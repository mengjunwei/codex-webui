/**
 * REST controller for aggregated Codex app-server status.
 */
import { Controller, Get } from '@nestjs/common';
import {
  ApiBearerAuth,
  ApiExtraModels,
  ApiOkResponse,
  ApiOperation,
  ApiTags,
  ApiUnauthorizedResponse,
} from '@nestjs/swagger';
import { ApiErrorResponseDto } from '../common/dto/api-responses.dto';
import { CodexStatusService, type CodexStatusResponse } from './codex-status.service';
import {
  CodexAccountStatusDto,
  CodexAppServerStatusDto,
  CodexConfigStatusDto,
  CodexInitializeStatusDto,
  CodexModelsStatusDto,
  CodexProviderStatusDto,
  CodexRuntimeStatusDto,
  CodexStatusErrorDto,
  CodexStatusResponseDto,
} from './dto/codex-status.dto';

@ApiTags('codex')
@ApiBearerAuth()
@ApiExtraModels(
  CodexStatusErrorDto,
  CodexAppServerStatusDto,
  CodexInitializeStatusDto,
  CodexAccountStatusDto,
  CodexConfigStatusDto,
  CodexProviderStatusDto,
  CodexModelsStatusDto,
  CodexRuntimeStatusDto,
)
@ApiUnauthorizedResponse({ type: ApiErrorResponseDto })
@Controller('codex')
export class CodexStatusController {
  constructor(private readonly codexStatusService: CodexStatusService) {}

  /** Returns aggregated Codex app-server readiness and runtime status. */
  @Get('status')
  @ApiOperation({ summary: 'Get aggregated Codex runtime status' })
  @ApiOkResponse({ type: CodexStatusResponseDto })
  async getStatus(): Promise<CodexStatusResponse> {
    return this.codexStatusService.getStatus();
  }
}
