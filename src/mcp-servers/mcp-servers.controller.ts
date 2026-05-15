/** REST controller for MCP server status and reload operations. */
import {
  BadRequestException,
  Controller,
  Get,
  HttpCode,
  HttpStatus,
  Post,
  Query,
} from '@nestjs/common';
import {
  ApiBadRequestResponse,
  ApiBearerAuth,
  ApiNoContentResponse,
  ApiOkResponse,
  ApiOperation,
  ApiQuery,
  ApiTags,
  ApiUnauthorizedResponse,
} from '@nestjs/swagger';
import { ApiErrorResponseDto } from '../common/dto/api-responses.dto';
import { McpServersService } from './mcp-servers.service';
import {
  MCP_SERVER_STATUS_DETAIL_VALUES,
  McpServersListResponseDto,
} from './dto/mcp-servers.dto';
import type { v2 } from '../codex/codex-schema';

@ApiTags('mcp-servers')
@ApiBearerAuth()
@ApiUnauthorizedResponse({ type: ApiErrorResponseDto })
@ApiBadRequestResponse({ type: ApiErrorResponseDto })
@Controller('mcp-servers')
export class McpServersController {
  constructor(private readonly mcpServersService: McpServersService) {}

  /** Lists MCP server status from app-server. */
  @Get()
  @ApiOperation({ summary: 'List MCP server status' })
  @ApiQuery({ name: 'cursor', required: false })
  @ApiQuery({ name: 'limit', required: false, type: Number })
  @ApiQuery({
    name: 'detail',
    required: false,
    enum: MCP_SERVER_STATUS_DETAIL_VALUES,
  })
  @ApiOkResponse({ type: McpServersListResponseDto })
  listServers(
    @Query('cursor') cursor?: string,
    @Query('limit') limit?: string,
    @Query('detail') detail?: string,
  ): Promise<v2.ListMcpServerStatusResponse> {
    return this.mcpServersService.listServers({
      cursor: cursor?.trim() || undefined,
      limit: this.parseLimit(limit),
      detail: this.parseDetail(detail),
    });
  }

  /** Reloads all configured MCP servers. */
  @Post('reload')
  @HttpCode(HttpStatus.NO_CONTENT)
  @ApiOperation({ summary: 'Reload all MCP servers' })
  @ApiNoContentResponse()
  reloadAll(): Promise<void> {
    return this.mcpServersService.reloadAll();
  }

  private parseLimit(value?: string): number | undefined {
    if (!value) return undefined;
    const limit = Number(value);
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) {
      throw new BadRequestException(
        'limit must be an integer between 1 and 100',
      );
    }
    return limit;
  }

  private parseDetail(value?: string): v2.McpServerStatusDetail | undefined {
    if (!value) return undefined;
    if (
      !(MCP_SERVER_STATUS_DETAIL_VALUES as readonly string[]).includes(value)
    ) {
      throw new BadRequestException('Invalid MCP server detail');
    }
    return value as v2.McpServerStatusDetail;
  }
}
