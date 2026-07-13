# backend-rs

codex-webui 后端的 Rust 重写版（替代 `../src` 中的 NestJS 后端）。

## 当前状态

占位说明。Cargo 工作区脚手架在实现计划的 **Phase 0** 落地。

## 参考资料

- 设计规格文档：`../docs/superpowers/specs/2026-07-06-codex-webui-rust-migration-design.md`
- 现有 TS 后端（迁移期间的参考基准）：`../src`
- 项目学习文档：`./STUDY.md`

## 目标（依据设计规格）

- **A** 性能与资源占用
- **B** 单一自包含二进制
- **C** 类型安全 + 长期可维护性
- 验收标准：可投入生产使用，行为与 TS 后端完全对齐。

API 契约（REST 路由、Socket.IO 命名空间与事件、OpenAPI operationId、错误码字符串）
逐字保留，因此 `../web` 中的 React 前端无需改动。
