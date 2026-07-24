//! Integration tests for AuthService.
//!
//! Verify: secret derivation matches TS HMAC formula, sign/verify roundtrip,
//! wrong key rejects, validate_api_key timing-safe, authenticate_token flows.

use codex_webui::auth::AuthService;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

fn expected_secret(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).unwrap();
    mac.update(b"codex-webui-jwt");
    hex::encode(mac.finalize().into_bytes())
}

#[test]
fn secret_derivation_matches_ts() {
    let svc = AuthService::new("my-secret-key");
    assert_eq!(svc.jwt_secret(), expected_secret("my-secret-key"));
}

#[test]
fn sign_verify_roundtrip() {
    let svc = AuthService::new("k");
    let resp = svc.sign_jwt().unwrap();
    assert!(svc.verify_jwt(&resp.access_token).unwrap());
    assert_eq!(resp.expires_in, 86_400);
}

#[test]
fn wrong_key_rejects_jwt() {
    let svc_a = AuthService::new("key-1");
    let svc_b = AuthService::new("key-2");
    let resp = svc_a.sign_jwt().unwrap();
    assert!(!svc_b.verify_jwt(&resp.access_token).unwrap());
}

#[test]
fn validate_api_key_correct_and_wrong() {
    let svc = AuthService::new("correct-horse");
    assert!(svc.validate_api_key("correct-horse"));
    assert!(!svc.validate_api_key("wrong"));
    assert!(!svc.validate_api_key(""));
}

#[test]
fn authenticate_token_jwt_then_apikey_then_invalid() {
    let svc = AuthService::new("my-deploy-key");
    let jwt = svc.sign_jwt().unwrap().access_token;

    // JWT succeeds.
    let r = svc.authenticate_token(Some(&jwt), None);
    assert!(r.ok);
    assert_eq!(r.auth_type.as_deref(), Some("jwt"));

    // Raw API key fallback.
    let r = svc.authenticate_token(Some("my-deploy-key"), None);
    assert!(r.ok);
    assert_eq!(r.auth_type.as_deref(), Some("apiKey"));

    // Invalid token.
    let r = svc.authenticate_token(Some("nope"), None);
    assert!(!r.ok);

    // Missing token.
    let r = svc.authenticate_token(None, None);
    assert!(!r.ok);
}
