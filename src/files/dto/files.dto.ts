import { ApiProperty, ApiPropertyOptional } from '@nestjs/swagger';

/** Directory entry returned by the file tree endpoint. */
export class FileEntryDto {
  @ApiProperty()
  name!: string;

  @ApiProperty()
  path!: string;

  @ApiProperty({ enum: ['file', 'directory'] })
  type!: 'file' | 'directory';

  @ApiPropertyOptional()
  size?: number;

  @ApiPropertyOptional()
  mtime?: number;
}

/** Text-file read response. */
export class FileReadResponseDto {
  @ApiProperty()
  content!: string;

  @ApiProperty()
  size!: number;
}

/** File write request body. */
export class WriteFileRequestDto {
  @ApiProperty()
  path!: string;

  @ApiProperty()
  content!: string;

  @ApiPropertyOptional()
  expectedMtime?: number;
}

/** File write response containing the new modification time. */
export class WriteFileResponseDto {
  @ApiProperty()
  mtime!: number;
}

/** File or directory metadata response. */
export class FileMetadataDto {
  @ApiProperty()
  path!: string;

  @ApiProperty()
  name!: string;

  @ApiProperty({ enum: ['file', 'directory', 'symlink', 'other'] })
  type!: 'file' | 'directory' | 'symlink' | 'other';

  @ApiProperty()
  size!: number;

  @ApiProperty()
  mtime!: number;

  @ApiProperty()
  permissions!: string;
}

/** Workspace roots response used by login and file browser bootstrapping. */
export class WorkspaceRootsResponseDto {
  @ApiProperty({ type: [String] })
  roots!: string[];

  @ApiProperty()
  homeDir!: string;
}

/** Request body for adding a workspace root. */
export class AddWorkspaceRootRequestDto {
  @ApiProperty()
  root!: string;
}
