import { Module } from '@nestjs/common';
import { AuthModule } from '../auth/auth.module';
import { ChatModule } from '../chat/chat.module';
import { CodexModule } from '../codex/codex.module';
import { FilesModule } from '../files/files.module';
import { PendingApprovalsModule } from '../pending-approvals/pending-approvals.module';
import { ActiveThreadRegistryService } from './active-thread-registry.service';
import { AutoResumeService } from './auto-resume.service';
import { ThreadResumeRegistryService } from './thread-resume-registry.service';
import { ThreadsController } from './threads.controller';
import { ThreadsGateway } from './threads.gateway';
import { ThreadsService } from './threads.service';

@Module({
  imports: [
    AuthModule,
    ChatModule,
    CodexModule,
    FilesModule,
    PendingApprovalsModule,
  ],
  controllers: [ThreadsController],
  providers: [
    ThreadsService,
    ThreadsGateway,
    ActiveThreadRegistryService,
    ThreadResumeRegistryService,
    AutoResumeService,
  ],
  exports: [
    ThreadsService,
    ActiveThreadRegistryService,
    ThreadResumeRegistryService,
  ],
})
export class ThreadsModule {}
