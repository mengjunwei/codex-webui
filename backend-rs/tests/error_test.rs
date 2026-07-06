//! Integration tests for error model.
//!
//! Response body must match `{ statusCode, errorCode, message, params? }`.
//! ErrorCode strings MUST be verbatim copies of `src/common/error-codes.ts`.

use axum::{http::StatusCode, response::IntoResponse};
use codex_webui::error::{AppError, ErrorCode};
use serde_json::Value;

// ── ErrorCode string parity ──────────────────────────────────────────────────

#[test]
fn error_code_strings_http() {
    assert_eq!(ErrorCode::HttpBadRequest.as_str(), "http.bad_request");
    assert_eq!(ErrorCode::HttpUnauthorized.as_str(), "http.unauthorized");
    assert_eq!(ErrorCode::HttpForbidden.as_str(), "http.forbidden");
    assert_eq!(ErrorCode::HttpNotFound.as_str(), "http.not_found");
    assert_eq!(ErrorCode::HttpConflict.as_str(), "http.conflict");
    assert_eq!(ErrorCode::HttpPayloadTooLarge.as_str(), "http.payload_too_large");
    assert_eq!(ErrorCode::HttpRequestFailed.as_str(), "http.request_failed");
    assert_eq!(ErrorCode::HttpInternalError.as_str(), "http.internal_error");
}

#[test]
fn error_code_strings_validation() {
    assert_eq!(ErrorCode::ValidationFieldRequired.as_str(), "validation.field_required");
    assert_eq!(ErrorCode::ValidationBodyRequired.as_str(), "validation.body_required");
    assert_eq!(ErrorCode::ValidationTypeMismatch.as_str(), "validation.type_mismatch");
    assert_eq!(ErrorCode::ValidationFieldInvalid.as_str(), "validation.field_invalid");
}

#[test]
fn error_code_strings_auth() {
    assert_eq!(ErrorCode::AuthMissingToken.as_str(), "auth.missing_token");
    assert_eq!(ErrorCode::AuthInvalidToken.as_str(), "auth.invalid_token");
    assert_eq!(ErrorCode::AuthMissingHeader.as_str(), "auth.missing_header");
    assert_eq!(ErrorCode::AuthInvalidApiKey.as_str(), "auth.invalid_api_key");
}

// ── Status fallback ──────────────────────────────────────────────────────────

#[test]
fn status_fallback_basic() {
    assert_eq!(ErrorCode::fallback_for(400), ErrorCode::HttpBadRequest);
    assert_eq!(ErrorCode::fallback_for(401), ErrorCode::HttpUnauthorized);
    assert_eq!(ErrorCode::fallback_for(500), ErrorCode::HttpInternalError);
    assert_eq!(ErrorCode::fallback_for(503), ErrorCode::HttpInternalError);
    assert_eq!(ErrorCode::fallback_for(418), ErrorCode::HttpRequestFailed);
}

// ── IntoResponse ─────────────────────────────────────────────────────────────

async fn body_json(resp: axum::response::Response) -> Value {
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn business_error_response() {
    let resp = AppError::business(
        ErrorCode::AuthInvalidApiKey,
        StatusCode::UNAUTHORIZED,
        "Invalid API key".into(),
        None,
    )
    .into_response();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp).await;
    assert_eq!(v["statusCode"], 401);
    assert_eq!(v["errorCode"], "auth.invalid_api_key");
    assert_eq!(v["message"], "Invalid API key");
    assert!(v["params"].is_null());
}

#[tokio::test]
async fn status_error_request_failed_with_params() {
    let resp = AppError::status(418).into_response();
    assert_eq!(resp.status(), StatusCode::from_u16(418).unwrap());
    let v = body_json(resp).await;
    assert_eq!(v["errorCode"], "http.request_failed");
    assert_eq!(v["params"]["status"], 418);
}

#[tokio::test]
async fn internal_error_is_500() {
    let resp = AppError::internal("boom".into()).into_response();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let v = body_json(resp).await;
    assert_eq!(v["errorCode"], "http.internal_error");
    assert_eq!(v["message"], "Internal server error");
}
