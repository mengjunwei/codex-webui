//! verify_access 多租户 access JWT 验签基线测试(WebSocket on_connect 依赖)。

use chrono::Utc;
use codex_webui::services::multitenant::auth::verify_access;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
    iat: usize,
    typ: String,
}

fn sign(secret: &str, sub: &str, typ: &str) -> String {
    let now = Utc::now().timestamp() as usize;
    let claims = Claims { sub: sub.into(), iat: now, exp: now + 900, typ: typ.into() };
    encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(secret.as_bytes())).unwrap()
}

#[test]
fn verify_access_returns_user_id_for_valid_mt_access_token() {
    let secret = "ws-test-secret";
    let token = sign(secret, "user-abc", "mt_access");
    let uid = verify_access(&token, secret).unwrap();
    assert_eq!(uid, "user-abc");
}

#[test]
fn verify_access_rejects_wrong_typ() {
    // typ != "mt_access"(如旧 sub="webui")→ 拒绝,WS 无法据此建立 user 身份
    let secret = "ws-test-secret";
    let token = sign(secret, "user-abc", "webui");
    assert!(verify_access(&token, secret).is_err());
}

#[test]
fn verify_access_rejects_bad_signature() {
    let token = sign("secret-a", "user-abc", "mt_access");
    assert!(verify_access(&token, "secret-b").is_err());
}
