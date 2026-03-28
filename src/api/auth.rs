use hmac::{Hmac, Mac};
use sha2::Sha384;

type HmacSha384 = Hmac<Sha384>;

fn hmac_sha384(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha384::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[allow(dead_code)]
pub fn sign_auth_payload(secret: &str, nonce: &str) -> String {
    hmac_sha384(secret, &format!("AUTH{}", nonce))
}

pub fn sign_rest_request(secret: &str, path: &str, nonce: &str, body: &str) -> String {
    hmac_sha384(secret, &format!("/api{}{}{}", path, nonce, body))
}
