//! Shared OAuth randomness: URL-safe secrets and the PKCE S256 challenge.
//!
//! Providers map the `getrandom` failure onto their own error type, so error
//! wording stays a provider decision while the entropy handling is written
//! once.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// `byte_count` bytes of operating-system entropy, URL-safe base64 without
/// padding.
pub fn random_url_safe(byte_count: usize) -> Result<String, getrandom::Error> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// A 32-byte random URL-safe token: the default size for PKCE verifiers,
/// flow ids, and OAuth state.
pub fn random_url_token() -> Result<String, getrandom::Error> {
    random_url_safe(32)
}

/// The PKCE S256 challenge for `verifier`: `base64url(SHA-256(verifier))`
/// without padding.
pub fn pkce_s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
