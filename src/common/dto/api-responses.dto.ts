import { ApiProperty, ApiPropertyOptional } from '@nestjs/swagger';

/** Standard success response for endpoints that only acknowledge an action. */
export class OkResponseDto {
  @ApiProperty({ example: true })
  ok!: boolean;
}

/** Standard NestJS HTTP error response shape. */
export class ApiErrorResponseDto {
  @ApiProperty({ example: 400 })
  statusCode!: number;

  @ApiProperty({
    oneOf: [
      { type: 'string', example: 'Bad Request' },
      { type: 'array', items: { type: 'string' } },
    ],
  })
  message!: string | string[];

  @ApiPropertyOptional({ example: 'Bad Request' })
  error?: string;
}
