//! 统一错误模型 —— ErrorCode、AppError 与 IntoResponse。
//!
//! 响应体：`{ statusCode, errorCode, message, params? }`
//! **errorCode 字符串必须与 `src/common/error-codes.ts` 逐字一致**
//! 因为 React 前端会将其用作 i18n 翻译键。
//!
//! 状态码回退表（与 `all-exceptions.filter.ts` 对齐）：
//! 400→http.bad_request, 401→http.unauthorized, 403→http.forbidden,
//! 404→http.not_found, 409→http.conflict, 413→http.payload_too_large,
//! 500→http.internal_error；其他 ≥500→http.internal_error；其他→http.request_failed
//! （附带 `params: { status }`）。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// 前端 i18n 错误码枚举。
/// 此处仅列出 Phase 0 模块所需的错误码；随着相应模块的迁移，
/// 其他错误码（files.*、codex.*、terminal.* 等）会逐步追加。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ErrorCode {
    // ── http.* ─────────────────────────────────────────────────────────
    HttpBadRequest,
    HttpUnauthorized,
    HttpForbidden,
    HttpNotFound,
    HttpConflict,
    HttpPayloadTooLarge,
    HttpRequestFailed,
    HttpInternalError,
    // ── validation.* ───────────────────────────────────────────────────
    ValidationFieldRequired,
    ValidationBodyRequired,
    ValidationTypeMismatch,
    ValidationFieldInvalid,
    // ── auth.* ─────────────────────────────────────────────────────────
    AuthMissingToken,
    AuthInvalidToken,
    AuthMissingHeader,
    AuthInvalidApiKey,
    // ── approvals.* ────────────────────────────────────────────────────
    ApprovalsNotFound,
    ApprovalsAlreadyResolved,
    ApprovalsServerNotConnected,
    ApprovalsAlreadyHandled,
    ApprovalsResultRequired,
    // ── account.* ──────────────────────────────────────────────────────
    AccountLoginIdRequired,
    AccountApiKeyRequired,
    AccountAccessTokenRequired,
    AccountChatgptAccountIdRequired,
    AccountInvalidLoginType,
    // ── skills.* ───────────────────────────────────────────────────────
    SkillsCwdRequired,
    SkillsPathOrNameRequired,
    // ── mcp.* ──────────────────────────────────────────────────────────
    McpInvalidServerDetail,
    McpScopesInvalid,
    McpScopesEmpty,
    McpTimeoutInvalid,
    McpTimeoutTooLarge,
    // ── plugins.* ──────────────────────────────────────────────────────
    PluginsFieldRequired,
    // ── threads.* ──────────────────────────────────────────────────────
    ThreadsInvalidLimit,
    ThreadsInvalidSortKey,
    ThreadsInvalidModel,
    ThreadsInvalidEffort,
    ThreadsInvalidRollbackTurns,
    ThreadsInvalidName,
    ThreadsInvalidInput,
    ThreadsInvalidInputItem,
    ThreadsInvalidInputUrl,
    ThreadsInvalidInputField,
    ThreadsInvalidInputType,
    // ── threads.*（配置热重载校验）──
    ThreadsInvalidApprovalPolicy,
    ThreadsInvalidSandboxMode,
    // ── codex.*（配置编辑）──
    CodexRawContentInvalid,
    CodexEditsNotArray,
    CodexEditInvalid,
    CodexKeyUnsupported,
    CodexValueInvalid,
    CodexValueInvalidJson,
    CodexWriteFailed,
    // ── files.* ────────────────────────────────────────────────────────
    FilesPathRequired,
    FilesPathNotFound,
    FilesPathOutsideWorkspace,
    FilesPathTraversal,
    FilesPathIsDirectory,
    FilesPathIsNotDirectory,
    FilesPathExists,
    FilesNameRequired,
    FilesNameInvalid,
    FilesContentRequired,
    FilesSourceAndDestRequired,
    FilesDestRequired,
    FilesWorkspaceRootNotDir,
    FilesParentNotDir,
    FilesNoParentFound,
    FilesDirNotEmpty,
    FilesNotWritable,
    FilesNotDownloadable,
    FilesFileTooLarge,
    FilesModifiedSinceRead,
    FilesCannotOverwriteDir,
    FilesUploadDirRequired,
    FilesUploadFileRequired,
    FilesUploadTooLarge,
    FilesInsufficientSpace,
    FilesCannotModifyRoot,
    FilesUploadPathInvalid,
    FilesMultipartUnavailable,
    FilesOverwriteDisabled,
    FilesOperationFailed,
    FilesPathExistsNotDir,
    // ── archive.* ─────────────────────────────────────────────────────
    ArchiveInvalidEntryPath,
    ArchiveEntryNotFound,
    ArchiveEntryNotFile,
    ArchiveEntryEncrypted,
    ArchiveEntryUnsupported,
    ArchiveEntrySizeUnknown,
    ArchiveEntryTooLarge,
    ArchivePathNotFile,
    ArchiveUnsupportedFormat,
    ArchiveTooManyEntries,
    ArchiveUnsafeEntryPath,
    ArchiveTotalSizeTooLarge,
    ArchiveSevenZipUnavailable,
    ArchiveRarUnavailable,
    ArchiveRarEntryNoStream,
    // ── settings.* (补充) ─────────────────────────────────────────────
    SettingsUpdatesRequired,
    SettingsKeyRequired,
    SettingsNotInEnum,
    // ── settings.* ─────────────────────────────────────────────────────
    SettingsNotFound,
    SettingsInvalidCategory,
    SettingsDuplicateKey,
    SettingsOutOfRange,
    SettingsInvalidValue,
    // ── chat.* ─────────────────────────────────────────────────────────
    ChatMultipartUnavailable,
    ChatFileRequired,
    ChatFilenameRequired,
    ChatImageOutsideRoot,
    ChatImageNotFile,
    ChatUploadNotFound,
    ChatImageAbsolutePath,
    ChatImagePathRequired,
    ChatFileInvalid,
    // ── onlyoffice.* ───────────────────────────────────────────────────
    OnlyOfficeNotConfigured,
    OnlyOfficeJwtRequired,
    OnlyOfficeFileRequired,
    OnlyOfficeUnsupportedFormat,
    OnlyOfficeMissingCallbackState,
    OnlyOfficeInvalidCallbackState,
    OnlyOfficeInvalidCallbackStatePayload,
    OnlyOfficeMissingCallbackJwt,
    OnlyOfficeInvalidCallbackJwt,
    OnlyOfficeInvalidDownloadUrl,
    OnlyOfficeDownloadUrlNotHttps,
    OnlyOfficeDownloadUrlOriginMismatch,
    OnlyOfficeSaveTooLarge,
    OnlyOfficeSaveNoBody,
    OnlyOfficeInvalidUrl,
    OnlyOfficePublicHostRequired,
    // ── terminal.* ─────────────────────────────────────────────────────
    TerminalMaxSessionsReached,
    TerminalExited,
    TerminalInputTooLarge,
    TerminalInvalidContext,
    TerminalInvalidCwd,
    TerminalCwdRequired,
    TerminalCwdNotDirectory,
    TerminalClosed,
    TerminalNotFound,
    TerminalContextMismatch,
    TerminalSocketNotAttached,
    // 后续模块追加于此。
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HttpBadRequest => "http.bad_request",
            Self::HttpUnauthorized => "http.unauthorized",
            Self::HttpForbidden => "http.forbidden",
            Self::HttpNotFound => "http.not_found",
            Self::HttpConflict => "http.conflict",
            Self::HttpPayloadTooLarge => "http.payload_too_large",
            Self::HttpRequestFailed => "http.request_failed",
            Self::HttpInternalError => "http.internal_error",
            Self::ValidationFieldRequired => "validation.field_required",
            Self::ValidationBodyRequired => "validation.body_required",
            Self::ValidationTypeMismatch => "validation.type_mismatch",
            Self::ValidationFieldInvalid => "validation.field_invalid",
            Self::AuthMissingToken => "auth.missing_token",
            Self::AuthInvalidToken => "auth.invalid_token",
            Self::AuthMissingHeader => "auth.missing_header",
            Self::AuthInvalidApiKey => "auth.invalid_api_key",
            Self::ApprovalsNotFound => "approvals.not_found",
            Self::ApprovalsAlreadyResolved => "approvals.already_resolved",
            Self::ApprovalsServerNotConnected => "approvals.server_not_connected",
            Self::ApprovalsAlreadyHandled => "approvals.already_handled",
            Self::ApprovalsResultRequired => "approvals.result_required",
            Self::AccountLoginIdRequired => "account.login_id_required",
            Self::AccountApiKeyRequired => "account.api_key_required",
            Self::AccountAccessTokenRequired => "account.access_token_required",
            Self::AccountChatgptAccountIdRequired => "account.chatgpt_account_id_required",
            Self::AccountInvalidLoginType => "account.invalid_login_type",
            Self::SkillsCwdRequired => "skills.cwd_required",
            Self::SkillsPathOrNameRequired => "skills.path_or_name_required",
            Self::McpInvalidServerDetail => "mcp.invalid_server_detail",
            Self::McpScopesInvalid => "mcp.scopes_invalid",
            Self::McpScopesEmpty => "mcp.scopes_empty",
            Self::McpTimeoutInvalid => "mcp.timeout_invalid",
            Self::McpTimeoutTooLarge => "mcp.timeout_too_large",
            Self::PluginsFieldRequired => "plugins.field_required",
            Self::ThreadsInvalidLimit => "threads.invalid_limit",
            Self::ThreadsInvalidSortKey => "threads.invalid_sort_key",
            Self::ThreadsInvalidModel => "threads.invalid_model",
            Self::ThreadsInvalidEffort => "threads.invalid_effort",
            Self::ThreadsInvalidRollbackTurns => "threads.invalid_rollback_turns",
            Self::ThreadsInvalidName => "threads.invalid_name",
            Self::ThreadsInvalidInput => "threads.invalid_input",
            Self::ThreadsInvalidInputItem => "threads.invalid_input_item",
            Self::ThreadsInvalidInputUrl => "threads.invalid_input_url",
            Self::ThreadsInvalidInputField => "threads.invalid_input_field",
            Self::ThreadsInvalidInputType => "threads.invalid_input_type",
            Self::ThreadsInvalidApprovalPolicy => "threads.invalid_approval_policy",
            Self::ThreadsInvalidSandboxMode => "threads.invalid_sandbox_mode",
            Self::CodexRawContentInvalid => "codex.raw_content_invalid",
            Self::CodexEditsNotArray => "codex.edits_not_array",
            Self::CodexEditInvalid => "codex.edit_invalid",
            Self::CodexKeyUnsupported => "codex.key_unsupported",
            Self::CodexValueInvalid => "codex.value_invalid",
            Self::CodexValueInvalidJson => "codex.value_invalid_json",
            Self::CodexWriteFailed => "codex.write_failed",
            Self::FilesPathRequired => "files.path_required",
            Self::FilesPathNotFound => "files.path_not_found",
            Self::FilesPathOutsideWorkspace => "files.path_outside_workspace",
            Self::FilesPathTraversal => "files.path_traversal",
            Self::FilesPathIsDirectory => "files.path_is_directory",
            Self::FilesPathIsNotDirectory => "files.path_is_not_directory",
            Self::FilesPathExists => "files.path_exists",
            Self::FilesNameRequired => "files.name_required",
            Self::FilesNameInvalid => "files.name_invalid",
            Self::FilesContentRequired => "files.content_required",
            Self::FilesSourceAndDestRequired => "files.source_and_dest_required",
            Self::FilesDestRequired => "files.dest_required",
            Self::FilesWorkspaceRootNotDir => "files.workspace_root_not_dir",
            Self::FilesParentNotDir => "files.parent_not_dir",
            Self::FilesNoParentFound => "files.no_parent_found",
            Self::FilesDirNotEmpty => "files.dir_not_empty",
            Self::FilesNotWritable => "files.not_writable",
            Self::FilesNotDownloadable => "files.not_downloadable",
            Self::FilesFileTooLarge => "files.file_too_large",
            Self::FilesModifiedSinceRead => "files.modified_since_read",
            Self::FilesCannotOverwriteDir => "files.cannot_overwrite_dir",
            Self::FilesUploadDirRequired => "files.upload_dir_required",
            Self::FilesUploadFileRequired => "files.upload_file_required",
            Self::FilesUploadTooLarge => "files.upload_too_large",
            Self::FilesInsufficientSpace => "files.insufficient_space",
            Self::FilesCannotModifyRoot => "files.cannot_modify_root",
            Self::FilesUploadPathInvalid => "files.upload_path_invalid",
            Self::FilesMultipartUnavailable => "files.multipart_unavailable",
            Self::FilesOverwriteDisabled => "files.overwrite_disabled",
            Self::FilesOperationFailed => "files.operation_failed",
            Self::FilesPathExistsNotDir => "files.path_exists_not_dir",
            Self::ArchiveInvalidEntryPath => "archive.invalid_entry_path",
            Self::ArchiveEntryNotFound => "archive.entry_not_found",
            Self::ArchiveEntryNotFile => "archive.entry_not_file",
            Self::ArchiveEntryEncrypted => "archive.entry_encrypted",
            Self::ArchiveEntryUnsupported => "archive.entry_unsupported",
            Self::ArchiveEntrySizeUnknown => "archive.entry_size_unknown",
            Self::ArchiveEntryTooLarge => "archive.entry_too_large",
            Self::ArchivePathNotFile => "archive.path_not_file",
            Self::ArchiveUnsupportedFormat => "archive.unsupported_format",
            Self::ArchiveTooManyEntries => "archive.too_many_entries",
            Self::ArchiveUnsafeEntryPath => "archive.unsafe_entry_path",
            Self::ArchiveTotalSizeTooLarge => "archive.total_size_too_large",
            Self::ArchiveSevenZipUnavailable => "archive.seven_zip_unavailable",
            Self::ArchiveRarUnavailable => "archive.rar_unavailable",
            Self::ArchiveRarEntryNoStream => "archive.rar_entry_no_stream",
            Self::SettingsUpdatesRequired => "settings.updates_required",
            Self::SettingsKeyRequired => "settings.key_required",
            Self::SettingsNotInEnum => "settings.not_in_enum",
            Self::SettingsNotFound => "settings.not_found",
            Self::SettingsInvalidCategory => "settings.invalid_category",
            Self::SettingsDuplicateKey => "settings.duplicate_key",
            Self::SettingsOutOfRange => "settings.out_of_range",
            Self::SettingsInvalidValue => "settings.invalid_value",
            Self::ChatMultipartUnavailable => "chat.multipart_unavailable",
            Self::ChatFileRequired => "chat.file_required",
            Self::ChatFilenameRequired => "chat.filename_required",
            Self::ChatImageOutsideRoot => "chat.image_outside_root",
            Self::ChatImageNotFile => "chat.image_not_file",
            Self::ChatUploadNotFound => "chat.upload_not_found",
            Self::ChatImageAbsolutePath => "chat.image_path_absolute",
            Self::ChatImagePathRequired => "chat.image_path_required",
            Self::ChatFileInvalid => "chat.file_invalid",
            Self::OnlyOfficeNotConfigured => "onlyoffice.not_configured",
            Self::OnlyOfficeJwtRequired => "onlyoffice.jwt_required",
            Self::OnlyOfficeFileRequired => "onlyoffice.file_required",
            Self::OnlyOfficeUnsupportedFormat => "onlyoffice.unsupported_format",
            Self::OnlyOfficeMissingCallbackState => "onlyoffice.missing_callback_state",
            Self::OnlyOfficeInvalidCallbackState => "onlyoffice.invalid_callback_state",
            Self::OnlyOfficeInvalidCallbackStatePayload => "onlyoffice.invalid_callback_state_payload",
            Self::OnlyOfficeMissingCallbackJwt => "onlyoffice.missing_callback_jwt",
            Self::OnlyOfficeInvalidCallbackJwt => "onlyoffice.invalid_callback_jwt",
            Self::OnlyOfficeInvalidDownloadUrl => "onlyoffice.invalid_download_url",
            Self::OnlyOfficeDownloadUrlNotHttps => "onlyoffice.download_url_not_https",
            Self::OnlyOfficeDownloadUrlOriginMismatch => "onlyoffice.download_url_origin_mismatch",
            Self::OnlyOfficeSaveTooLarge => "onlyoffice.save_too_large",
            Self::OnlyOfficeSaveNoBody => "onlyoffice.save_no_body",
            Self::OnlyOfficeInvalidUrl => "onlyoffice.invalid_url",
            Self::OnlyOfficePublicHostRequired => "onlyoffice.public_host_required",
            Self::TerminalMaxSessionsReached => "terminal.max_sessions_reached",
            Self::TerminalExited => "terminal.exited",
            Self::TerminalInputTooLarge => "terminal.input_too_large",
            Self::TerminalInvalidContext => "terminal.invalid_context",
            Self::TerminalInvalidCwd => "terminal.invalid_cwd",
            Self::TerminalCwdRequired => "terminal.cwd_required",
            Self::TerminalCwdNotDirectory => "terminal.cwd_not_directory",
            Self::TerminalClosed => "terminal.closed",
            Self::TerminalNotFound => "terminal.not_found",
            Self::TerminalContextMismatch => "terminal.context_mismatch",
            Self::TerminalSocketNotAttached => "terminal.socket_not_attached",
        }
    }

    /// 查找给定 HTTP 状态码对应的回退 ErrorCode。
    pub fn fallback_for(status: u16) -> Self {
        match status {
            400 => Self::HttpBadRequest,
            401 => Self::HttpUnauthorized,
            403 => Self::HttpForbidden,
            404 => Self::HttpNotFound,
            409 => Self::HttpConflict,
            413 => Self::HttpPayloadTooLarge,
            500 => Self::HttpInternalError,
            s if s >= 500 => Self::HttpInternalError,
            _ => Self::HttpRequestFailed,
        }
    }
}

/// 前端 i18n 的可选插值参数。
/// 与 TS 的 `ErrorParams = Record<string, string | number>` 对齐。
pub type Params = BTreeMap<String, serde_json::Value>;

/// 统一的应用错误类型。
#[derive(Debug)]
pub enum AppError {
    /// 带有显式 code + status + message 的结构化业务错误。
    Business {
        code: ErrorCode,
        status: StatusCode,
        message: Value,
        params: Option<Params>,
    },
    /// 仅包含 HTTP 状态码的错误；code 与 message 由状态码派生。
    Status { status: StatusCode },
    /// 未处理的内部错误；始终渲染为 500 + http.internal_error。
    Internal(String),
}

impl AppError {
    pub fn business(
        code: ErrorCode,
        status: StatusCode,
        message: String,
        params: Option<Params>,
    ) -> Self {
        Self::Business {
            code,
            status,
            message: Value::String(message),
            params,
        }
    }

    pub fn status(code: u16) -> Self {
        Self::Status {
            status: StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    pub fn internal(msg: String) -> Self {
        Self::Internal(msg)
    }

    pub fn unauthorized(code: ErrorCode, msg: &str) -> Self {
        Self::business(code, StatusCode::UNAUTHORIZED, msg.into(), None)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, params) = match self {
            Self::Business {
                code,
                status,
                message,
                params,
            } => (status, code, message, params),

            Self::Status { status } => {
                let code = ErrorCode::fallback_for(status.as_u16());
                let mut params = None;
                if matches!(code, ErrorCode::HttpRequestFailed) {
                    let mut m = Params::new();
                    m.insert("status".into(), serde_json::Value::Number(status.as_u16().into()));
                    params = Some(m);
                }
                (
                    status,
                    code,
                    Value::String(format!("Request failed ({})", status.as_u16())),
                    params,
                )
            }

            Self::Internal(ref msg) => {
                tracing::error!(error = %msg, "unhandled exception");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::HttpInternalError,
                    Value::String("Internal server error".into()),
                    None,
                )
            }
        };

        let mut body = json!({
            "statusCode": status.as_u16(),
            "errorCode": code.as_str(),
            "message": message,
        });
        if let Some(p) = params {
            body["params"] = json!(p);
        }

        (status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<axum::http::Error> for AppError {
    fn from(e: axum::http::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Business { message, code, .. } => write!(f, "{}: {}", code.as_str(), message),
            Self::Status { status } => write!(f, "HTTP {}", status),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}
