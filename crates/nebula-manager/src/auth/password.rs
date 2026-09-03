//! Password hashing.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;

use crate::error::{ApiError, ApiResult};

/// The shortest password accepted.
///
/// Length is the only property worth enforcing: composition rules push people
/// towards predictable substitutions without adding real entropy.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Hash a password with Argon2id and a fresh random salt.
pub fn hash(password: &str) -> ApiResult<String> {
    validate(password)?;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("password hashing failed: {e}")))
}

/// Check a password against a stored hash.
///
/// Returns `false` rather than an error for a malformed stored hash: a
/// corrupt row must not become an authentication bypass, and the caller
/// treats both cases identically anyway.
#[must_use]
pub fn verify(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Spend the same work as a real verification against a throwaway hash.
///
/// Called when the account does not exist. Without it, a missing user answers
/// measurably faster than a wrong password, which turns login into a user
/// enumeration oracle.
pub fn verify_dummy(password: &str) {
    let _ = verify(password, &DUMMY);
}

/// A real hash of a random secret, computed once.
///
/// It must be genuinely well-formed: a hard-coded string that fails to parse
/// would make [`verify`] return early and reintroduce the very timing
/// difference this exists to remove. Deriving it at startup also means no
/// attacker knows the underlying password, so the comparison can never
/// accidentally succeed.
static DUMMY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let salt = SaltString::generate(&mut OsRng);
    let secret: [u8; 32] = rand::random();
    Argon2::default()
        .hash_password(&secret, &salt)
        .expect("hashing a fixed-length secret cannot fail")
        .to_string()
});

fn validate(password: &str) -> ApiResult<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(ApiError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > 1024 {
        // Argon2 is deliberately expensive; an unbounded input is a cheap
        // way for a caller to burn server CPU.
        return Err(ApiError::BadRequest("password is too long".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("Correct horse battery staple", &h));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A shared salt would let one rainbow table crack every account.
        let a = hash("correct horse battery staple").unwrap();
        let b = hash("correct horse battery staple").unwrap();
        assert_ne!(a, b);
        assert!(verify("correct horse battery staple", &a));
        assert!(verify("correct horse battery staple", &b));
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(hash("short").is_err());
        assert!(hash(&"a".repeat(MIN_PASSWORD_LEN)).is_ok());
    }

    #[test]
    fn absurdly_long_passwords_are_refused() {
        assert!(hash(&"a".repeat(2000)).is_err());
    }

    #[test]
    fn a_corrupt_stored_hash_never_verifies() {
        assert!(!verify("anything", "not-a-hash"));
        assert!(!verify("anything", ""));
    }

    #[test]
    fn the_dummy_hash_is_a_real_one() {
        // If this stopped parsing, `verify_dummy` would bail out early and
        // the login path would leak whether an account exists.
        verify_dummy("whatever");
        assert!(PasswordHash::new(&DUMMY).is_ok());
        assert!(!verify("whatever", &DUMMY));
    }
}
