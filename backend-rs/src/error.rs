//! Unified error model — ErrorCode, AppError, and IntoResponse.
//!
//! Response body: `{ statusCode, errorCode, message, params? }`
//! **errorCode strings MUST be verbatim copies of `src/common/error-codes.ts`**
//! because the React frontend uses them as i18n translation keys.
//!
//! Status fallback table (parity with `all-exceptions.filter.ts`):
//! 400→http.bad_request, 401→http.unauthorized, 403→http.forbidden,
//! 404→http.not_found, 409→http.conflict, 413→http.payload_too_large,
//! 500→http.internal_error; other ≥500→http.internal_error; other→http.request_failed
//! (with `params: { status }`).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Frontend i18n error code enum.
/// Only codes needed by Phase 0 modules are listed here; additional codes
/// (files.*, codex.*, terminal.*, etc.) are appended as those modules are ported.
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
    // ── threads.* (config hot-reload validation) ──
    ThreadsInvalidApprovalPolicy,
    ThreadsInvalidSandboxMode,
    // ── codex.* (config editing) ──
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
    // Phase 2+: terminal.*, etc. — appended here.
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
        }
    }

    /// Lookup the fallback ErrorCode for a given HTTP status code.
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

/// Optional interpolation params for frontend i18n.
/// Parity with TS `ErrorParams = Record<string, string | number>`.
pub type Params = BTreeMap<String, serde_json::Value>;

/// Unified application error type.
#[derive(Debug)]
pub enum AppError {
    /// Structured business error with explicit code + status + message.
    Business {
        code: ErrorCode,
        status: StatusCode,
        message: Value,
        params: Option<Params>,
    },
    /// HTTP status-only error; code and message are derived from the status.
    Status { status: StatusCode },
    /// Unhandled internal error; always renders as 500 + http.internal_error.
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
