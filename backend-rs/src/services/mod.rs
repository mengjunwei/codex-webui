//! Service 层：业务逻辑编排。
//!
//! 注：`TerminalService` 和 `ThreadResumeRegistry` 目前定义在 api/ 层的混合文件中，
//! 此处 re-export 以保持三层架构的 import 路径一致性。
//! 后续可将纯 service 逻辑拆分到此目录下的独立文件。

pub mod codex_status;
pub mod codex_status_config;
pub mod files;
pub mod multitenant;
pub mod settings;

// Re-export：从 api 层的混合文件中导出 service 类型。
pub use crate::api::terminal as terminal;
pub use crate::api::threads as threads;
