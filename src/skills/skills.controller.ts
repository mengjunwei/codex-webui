/** REST controller for Codex skills inventory. */
import { BadRequestException, Controller, Get, Query } from '@nestjs/common';
import {
  ApiBadRequestResponse,
  ApiBearerAuth,
  ApiOkResponse,
  ApiOperation,
  ApiQuery,
  ApiTags,
  ApiUnauthorizedResponse,
} from '@nestjs/swagger';
import type { v2 } from '../codex/codex-schema';
import { ApiErrorResponseDto } from '../common/dto/api-responses.dto';
import { SkillsListResponseDto } from './dto/skills.dto';
import { SkillsService } from './skills.service';

@ApiTags('skills')
@ApiBearerAuth()
@ApiUnauthorizedResponse({ type: ApiErrorResponseDto })
@ApiBadRequestResponse({ type: ApiErrorResponseDto })
@Controller('skills')
export class SkillsController {
  constructor(private readonly skillsService: SkillsService) {}

  /** Proxies skills/list for the requested cwd and returns the raw Codex response. */
  @Get()
  @ApiOperation({ summary: 'List Codex skills for a working directory' })
  @ApiQuery({ name: 'cwd', required: true })
  @ApiOkResponse({ type: SkillsListResponseDto })
  listSkills(@Query('cwd') cwd?: string): Promise<v2.SkillsListResponse> {
    const normalizedCwd = cwd?.trim();
    if (!normalizedCwd) {
      throw new BadRequestException('cwd is required');
    }
    return this.skillsService.listSkills({ cwds: [normalizedCwd] });
  }
}
