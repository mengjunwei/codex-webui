/** Turn error persistence module. */
import { Module } from '@nestjs/common';
import { CodexModule } from '../codex/codex.module';
import { DatabaseModule } from '../database/database.module';
import { TurnErrorsController } from './turn-errors.controller';
import { TurnErrorsService } from './turn-errors.service';

@Module({
  imports: [CodexModule, DatabaseModule],
  controllers: [TurnErrorsController],
  providers: [TurnErrorsService],
  exports: [TurnErrorsService],
})
export class TurnErrorsModule {}
