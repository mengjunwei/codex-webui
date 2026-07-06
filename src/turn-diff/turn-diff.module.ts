/** Turn diff persistence module. */
import { Module } from '@nestjs/common';
import { CodexModule } from '../codex/codex.module';
import { DatabaseModule } from '../database/database.module';
import { TurnDiffController } from './turn-diff.controller';
import { TurnDiffService } from './turn-diff.service';

@Module({
  imports: [CodexModule, DatabaseModule],
  controllers: [TurnDiffController],
  providers: [TurnDiffService],
  exports: [TurnDiffService],
})
export class TurnDiffModule {}
