import { Module } from '@nestjs/common';
import { FilesController } from './files.controller';
import { FilesGateway } from './files.gateway';
import { FilesService } from './files.service';

@Module({
  controllers: [FilesController],
  providers: [FilesService, FilesGateway],
  exports: [FilesService],
})
export class FilesModule {}
