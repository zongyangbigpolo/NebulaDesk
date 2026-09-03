//! Long-lived identity keys.

use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine as _;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{CryptoError, Result};

/// Length of an X25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;
/// Length of an X25519 secret key.
pub const SECRET_KEY_LEN: usize = 32;

/// An X25519 public key — an agent's stable cryptographic identity.
///
/// The manager stores this at enrolment and hands it to authorised clients
/// inside their session ticket.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; PUBLIC_KEY_LEN]);

impl PublicKey {
    /// Wrap raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; PUBLIC_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.0
    }

    /// Parse from a slice of exactly [`PUBLIC_KEY_LEN`] bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; PUBLIC_KEY_LEN] = bytes.try_into().map_err(|_| {
            CryptoError::InvalidKey(format!(
                "public key must be {PUBLIC_KEY_LEN} bytes, got {}",
                bytes.len()
            ))
        })?;
        Ok(Self(arr))
    }

    /// Render as unpadded base64, the form used in the API and database.
    #[must_use]
    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    /// Parse from unpadded base64.
    pub fn from_base64(s: &str) -> Result<Self> {
        let raw = B64
            .decode(s.trim())
            .map_err(|e| CryptoError::InvalidKey(format!("bad base64 public key: {e}")))?;
        Self::from_slice(&raw)
    }

    /// A short, human-comparable fingerprint for logs and UI.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let hex = hex::encode(self.0);
        format!("{}:{}", &hex[..8], &hex[56..])
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_base64())
    }
}

/// An X25519 keypair. The secret half is zeroed on drop.
#[derive(Clone, ZeroizeOnDrop)]
pub struct StaticKeypair {
    secret: [u8; SECRET_KEY_LEN],
    #[zeroize(skip)]
    public: PublicKey,
}

impl StaticKeypair {
    /// Generate a fresh keypair from the OS CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let params = crate::handshake::noise_params();
        let kp = snow::Builder::new(params)
            .generate_keypair()
            .expect("x25519 keygen cannot fail");
        let mut secret = [0u8; SECRET_KEY_LEN];
        secret.copy_from_slice(&kp.private);
        let mut public = [0u8; PUBLIC_KEY_LEN];
        public.copy_from_slice(&kp.public);
        Self {
            secret,
            public: PublicKey(public),
        }
    }

    /// Reconstruct from a stored secret key.
    pub fn from_secret(secret_bytes: &[u8]) -> Result<Self> {
        let secret: [u8; SECRET_KEY_LEN] = secret_bytes.try_into().map_err(|_| {
            CryptoError::InvalidKey(format!(
                "secret key must be {SECRET_KEY_LEN} bytes, got {}",
                secret_bytes.len()
            ))
        })?;
        // Derive the public half by a scalar multiplication with the base
        // point, which snow exposes through a throwaway handshake build.
        let public = x25519_public_from_secret(&secret);
        Ok(Self {
            secret,
            public: PublicKey(public),
        })
    }

    /// The public half.
    #[must_use]
    pub const fn public(&self) -> PublicKey {
        self.public
    }

    /// The secret half. Handle with care; it is zeroed when the pair drops.
    #[must_use]
    pub const fn secret_bytes(&self) -> &[u8; SECRET_KEY_LEN] {
        &self.secret
    }

    /// Serialize the secret for on-disk storage (base64, unpadded).
    #[must_use]
    pub fn secret_to_base64(&self) -> String {
        B64.encode(self.secret)
    }

    /// Load from the base64 form produced by [`Self::secret_to_base64`].
    pub fn from_secret_base64(s: &str) -> Result<Self> {
        let raw = B64
            .decode(s.trim())
            .map_err(|e| CryptoError::InvalidKey(format!("bad base64 secret key: {e}")))?;
        let mut kp = Self::from_secret(&raw)?;
        let mut scratch = raw;
        scratch.zeroize();
        kp.public = PublicKey(x25519_public_from_secret(&kp.secret));
        Ok(kp)
    }
}

impl std::fmt::Debug for StaticKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StaticKeypair({:?})", self.public)
    }
}

/// X25519 scalar multiplication against the base point.
fn x25519_public_from_secret(secret: &[u8; SECRET_KEY_LEN]) -> [u8; PUBLIC_KEY_LEN] {
    let sk = x25519_dalek::StaticSecret::from(*secret);
    x25519_dalek::PublicKey::from(&sk).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_distinct() {
        let a = StaticKeypair::generate();
        let b = StaticKeypair::generate();
        assert_ne!(a.public(), b.public());
    }

    #[test]
    fn public_key_survives_base64_roundtrip() {
        let kp = StaticKeypair::generate();
        let encoded = kp.public().to_base64();
        assert_eq!(PublicKey::from_base64(&encoded).unwrap(), kp.public());
    }

    #[test]
    fn keypair_survives_secret_roundtrip() {
        let kp = StaticKeypair::generate();
        let restored = StaticKeypair::from_secret_base64(&kp.secret_to_base64()).unwrap();
        assert_eq!(restored.public(), kp.public());
        assert_eq!(restored.secret_bytes(), kp.secret_bytes());
    }

    #[test]
    fn wrong_length_keys_are_rejected() {
        assert!(PublicKey::from_slice(&[0u8; 31]).is_err());
        assert!(StaticKeypair::from_secret(&[0u8; 16]).is_err());
        assert!(PublicKey::from_base64("!!!not base64!!!").is_err());
    }

    #[test]
    fn fingerprint_is_short_and_stable() {
        let key = PublicKey([0xab; PUBLIC_KEY_LEN]);
        assert_eq!(key.fingerprint(), "abababab:abababab");
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let kp = StaticKeypair::generate();
        let rendered = format!("{kp:?}");
        assert!(!rendered.contains(&hex::encode(kp.secret_bytes())));
    }
}
