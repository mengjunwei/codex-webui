/** Handles temporary uploads that become rich Codex chat inputs. */
import {
  BadRequestException,
  ForbiddenException,
  HttpException,
  Injectable,
  Logger,
  NotFoundException,
  PayloadTooLargeException,
} from '@nestjs/common';
import { ConfigService } from '@nestjs/config';
import { randomUUID } from 'node:crypto';
import * as fs from 'node:fs/promises';
import * as fsSync from 'node:fs';
import { homedir } from 'node:os';
import * as path from 'node:path';
import type { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { FILES_SETTING_KEYS } from '../settings/settings.definitions';
import { SettingsService } from '../settings/settings.service';

const CHAT_UPLOAD_DIR_NAME = 'webui-uploads';
const CHAT_UPLOAD_TTL_MS = 24 * 60 * 60 * 1000;
const CHAT_UPLOAD_SWEEP_INTERVAL_MS = 60 * 60 * 1000;
const MAX_EXTENSION_LENGTH = 32;

export interface ChatUploadInput {
  filename: string;
  mimeType?: string;
  stream: Readable & { truncated?: boolean };
}

export interface ChatUploadResult {
  path: string;
  size: number;
  mimeType: string;
}

@Injectable()
export class ChatUploadService {
  private readonly logger = new Logger(ChatUploadService.name);
  private lastSweepMs = 0;

  constructor(
    private readonly configService: ConfigService,
    private readonly settingsService: SettingsService,
  ) {}

  /** Returns the current upload byte limit from runtime settings. */
  getUploadMaxBytes(): number {
    return this.settingsService.getNumberSetting(
      FILES_SETTING_KEYS.uploadMaxBytes,
    );
  }

  /** Streams one browser-uploaded attachment into the Codex-readable staging directory. */
  async saveUploadedFile(upload: ChatUploadInput): Promise<ChatUploadResult> {
    this.validateUploadFile(upload);

    const uploadRoot = await this.ensureUploadRoot();
    await this.sweepExpiredUploads(uploadRoot).catch((error: unknown) => {
      const message = error instanceof Error ? error.message : String(error);
      this.logger.warn(`Chat upload cleanup failed: ${message}`);
    });

    const extension = this.getSafeExtension(upload.filename);
    const targetPath = path.join(uploadRoot, `${randomUUID()}${extension}`);
    let tempPath: string | null = path.join(uploadRoot, `.${randomUUID()}.tmp`);

    try {
      await pipeline(
        upload.stream,
        fsSync.createWriteStream(tempPath, { flags: 'wx', mode: 0o600 }),
      );

      if (upload.stream.truncated) {
        throw new PayloadTooLargeException(
          'Uploaded file exceeds maximum size',
        );
      }

      await fs.rename(tempPath, targetPath);
      tempPath = null;
      const stat = await fs.stat(targetPath);
      return {
        path: targetPath,
        size: stat.size,
        mimeType: upload.mimeType?.trim() || 'application/octet-stream',
      };
    } catch (error) {
      if (tempPath) {
        await fs.rm(tempPath, { force: true }).catch(() => undefined);
      }
      await fs.rm(targetPath, { force: true }).catch(() => undefined);
      this.rethrowUploadError(error, targetPath);
    }
  }

  /** Resolves a previously staged local image path and verifies it remains inside upload root. */
  async resolveStoredUploadPath(inputPath: string): Promise<string> {
    if (typeof inputPath !== 'string' || inputPath.trim().length === 0) {
      throw new BadRequestException('localImage path is required');
    }

    const absoluteInputPath = inputPath.trim();
    if (!path.isAbsolute(absoluteInputPath)) {
      throw new BadRequestException('localImage path must be absolute');
    }

    const uploadRoot = await this.ensureUploadRoot();
    let resolvedPath: string;
    try {
      resolvedPath = await fs.realpath(absoluteInputPath);
    } catch {
      throw new NotFoundException(`Upload not found: ${absoluteInputPath}`);
    }

    if (!this.isPathInside(resolvedPath, uploadRoot)) {
      throw new ForbiddenException('localImage path outside chat upload root');
    }

    const stat = await fs.stat(resolvedPath);
    if (!stat.isFile()) {
      throw new BadRequestException('localImage path must be a file');
    }
    return resolvedPath;
  }

  /** Creates and resolves the dedicated CODEX_HOME upload directory. */
  private async ensureUploadRoot(): Promise<string> {
    const uploadRoot = this.getUploadRootPath();
    await fs.mkdir(uploadRoot, { recursive: true, mode: 0o700 });
    return fs.realpath(uploadRoot);
  }

  /** Resolves CODEX_HOME the same way as the database default path. */
  private getUploadRootPath(): string {
    const codexHome = this.configService.get<string>('CODEX_HOME')?.trim();
    const baseDir = codexHome || path.join(homedir(), '.codex');
    return path.join(baseDir, CHAT_UPLOAD_DIR_NAME);
  }

  /** Rejects invalid multipart file metadata before using any client-controlled filename. */
  private validateUploadFile(upload: ChatUploadInput): void {
    if (!upload || typeof upload.filename !== 'string') {
      throw new BadRequestException('Uploaded file is required');
    }
    const filename = upload.filename.trim();
    if (filename.length === 0) {
      throw new BadRequestException('Uploaded file filename is required');
    }
    if (
      filename.includes('/') ||
      filename.includes('\\') ||
      filename.includes('\0')
    ) {
      throw new BadRequestException(
        'Uploaded file filename must not contain path separators',
      );
    }
  }

  /** Preserves only a safe extension; the stored basename is always generated by WebUI. */
  private getSafeExtension(filename: string): string {
    const extension = path.extname(filename.trim());
    if (!extension || extension.length > MAX_EXTENSION_LENGTH) {
      return '';
    }
    if (!/^\.[A-Za-z0-9][A-Za-z0-9._-]*$/.test(extension)) {
      return '';
    }
    return extension;
  }

  /** Opportunistically prunes old staged upload files without blocking new uploads on errors. */
  private async sweepExpiredUploads(uploadRoot: string): Promise<void> {
    const nowMs = Date.now();
    if (nowMs - this.lastSweepMs < CHAT_UPLOAD_SWEEP_INTERVAL_MS) {
      return;
    }
    this.lastSweepMs = nowMs;

    const entries = await fs.readdir(uploadRoot, { withFileTypes: true });
    const cleanupTasks = entries.map(async (entry) => {
      if (!entry.isFile()) {
        return;
      }
      const entryPath = path.join(uploadRoot, entry.name);
      const stat = await fs.stat(entryPath);
      if (nowMs - stat.mtimeMs > CHAT_UPLOAD_TTL_MS) {
        await fs.rm(entryPath, { force: true });
      }
    });
    await Promise.all(cleanupTasks);
  }

  /** Converts filesystem and multipart failures into explicit HTTP exceptions. */
  private rethrowUploadError(error: unknown, targetPath: string): never {
    if (error instanceof HttpException) {
      throw error;
    }
    if (this.isFileSizeLimitError(error)) {
      throw new PayloadTooLargeException('Uploaded file exceeds maximum size');
    }

    const code = this.getErrorCode(error);
    if (code === 'ENOENT') {
      throw new NotFoundException(`Path not found: ${targetPath}`);
    }
    if (code === 'EACCES' || code === 'EPERM') {
      throw new BadRequestException('Upload directory is not writable');
    }
    if (code === 'ENOSPC') {
      throw new BadRequestException('Insufficient disk space for upload');
    }

    const message = error instanceof Error ? error.message : 'Upload failed';
    throw new BadRequestException(message);
  }

  /** Checks if an error came from multipart file-size enforcement. */
  private isFileSizeLimitError(error: unknown): boolean {
    const code = this.getErrorCode(error);
    if (code === 'FST_REQ_FILE_TOO_LARGE') {
      return true;
    }
    if (!(error instanceof Error)) {
      return false;
    }
    return (
      error.name.includes('FileTooLarge') ||
      error.message.toLowerCase().includes('file size')
    );
  }

  /** Extracts Node/Fastify-style error codes without weakening type safety. */
  private getErrorCode(error: unknown): string | undefined {
    if (typeof error !== 'object' || error === null || !('code' in error)) {
      return undefined;
    }
    const code = (error as { code?: unknown }).code;
    return typeof code === 'string' ? code : undefined;
  }

  /** Returns true when targetPath is the root or a descendant of rootPath. */
  private isPathInside(targetPath: string, rootPath: string): boolean {
    const relativePath = path.relative(rootPath, targetPath);
    return (
      relativePath === '' ||
      (!relativePath.startsWith('..') && !path.isAbsolute(relativePath))
    );
  }
}
