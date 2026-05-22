/**
 * Global exception filter that standardizes all HTTP error responses to:
 * { statusCode, errorCode, message, params? }
 *
 * - BusinessException: uses its errorCode + params directly.
 * - Other HttpException: falls back to a status-based error code.
 * - Unknown errors: 500 + http.internal_error.
 */
import {
  ArgumentsHost,
  Catch,
  ExceptionFilter,
  HttpException,
  Logger,
} from '@nestjs/common';
import type { FastifyReply } from 'fastify';
import { ErrorCode } from './error-codes';
import type { ErrorCodeValue } from './error-codes';
import { BusinessException } from './business.exception';

interface ErrorResponseBody {
  statusCode: number;
  errorCode: ErrorCodeValue;
  message: string | string[];
  params?: Record<string, string | number>;
}

/** Maps common HTTP status codes to fallback error codes. */
const STATUS_FALLBACK_CODES: Partial<Record<number, ErrorCodeValue>> = {
  400: ErrorCode.http.badRequest,
  401: ErrorCode.http.unauthorized,
  403: ErrorCode.http.forbidden,
  404: ErrorCode.http.notFound,
  409: ErrorCode.http.conflict,
  413: ErrorCode.http.payloadTooLarge,
  500: ErrorCode.http.internalError,
};

@Catch()
export class AllExceptionsFilter implements ExceptionFilter {
  private readonly logger = new Logger(AllExceptionsFilter.name);

  catch(exception: unknown, host: ArgumentsHost): void {
    // Skip non-HTTP contexts (WebSocket exceptions handled by NestJS gateway)
    if (host.getType() !== 'http') return;

    const response = host.switchToHttp().getResponse<FastifyReply>();

    if (exception instanceof BusinessException) {
      const status = exception.getStatus();
      const body: ErrorResponseBody = {
        statusCode: status,
        errorCode: exception.errorCode,
        message: exception.message,
      };
      if (exception.params) body.params = exception.params;
      void response.status(status).send(body);
      return;
    }

    if (exception instanceof HttpException) {
      const status = exception.getStatus();
      const message = this.normalizeExceptionMessage(exception);
      const errorCode =
        STATUS_FALLBACK_CODES[status] ??
        (status >= 500
          ? ErrorCode.http.internalError
          : ErrorCode.http.requestFailed);
      const body: ErrorResponseBody = {
        statusCode: status,
        errorCode,
        message,
      };
      if (errorCode === ErrorCode.http.requestFailed) {
        body.params = { status };
      }
      void response.status(status).send(body);
      return;
    }

    // Unknown / unhandled error
    const msg =
      exception instanceof Error ? exception.message : String(exception);
    this.logger.error({ error: msg }, 'Unhandled exception');
    void response.status(500).send({
      statusCode: 500,
      errorCode: ErrorCode.http.internalError,
      message: 'Internal server error',
    } satisfies ErrorResponseBody);
  }

  /** Safely extracts a string or string[] message from an HttpException response. */
  private normalizeExceptionMessage(
    exception: HttpException,
  ): string | string[] {
    const exResponse = exception.getResponse();
    if (typeof exResponse === 'string') return exResponse;
    if (typeof exResponse === 'object' && exResponse !== null) {
      const msg = (exResponse as Record<string, unknown>).message;
      if (typeof msg === 'string') return msg;
      if (Array.isArray(msg)) {
        const strings = msg.filter(
          (item): item is string => typeof item === 'string',
        );
        if (strings.length > 0) return strings;
      }
    }
    return exception.message;
  }
}
