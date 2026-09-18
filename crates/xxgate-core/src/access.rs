use crate::{Error, Result, accounts::Credentials};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub fn random_secret(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn secret_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.len() < 12 || password.len() > 1024 {
        return Err(Error::invalid("Password must contain 12 to 1024 bytes"));
    }
    Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|v| v.to_string())
        .map_err(|_| Error::new(500, "password_hash_failed", "Unable to hash password"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    if password.len() > 1024 {
        return false;
    }
    PasswordHash::new(hash).ok().is_some_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

#[derive(Clone)]
pub struct CredentialCipher(ChaCha20Poly1305);

impl CredentialCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(ChaCha20Poly1305::new(key.into()))
    }
    pub fn encrypt(&self, account_id: Uuid, credentials: &Credentials) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let plaintext =
            serde_json::to_vec(credentials).map_err(|_| Error::invalid("Invalid credentials"))?;
        let encrypted = self
            .0
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: account_id.as_bytes(),
                },
            )
            .map_err(|_| Error::new(500, "encryption_failed", "Unable to encrypt credentials"))?;
        Ok([nonce.as_slice(), encrypted.as_slice()].concat())
    }
    pub fn decrypt(&self, account_id: Uuid, encrypted: &[u8]) -> Result<Credentials> {
        if encrypted.len() < 28 {
            return Err(Error::storage());
        }
        let plaintext = self
            .0
            .decrypt(
                Nonce::from_slice(&encrypted[..12]),
                Payload {
                    msg: &encrypted[12..],
                    aad: account_id.as_bytes(),
                },
            )
            .map_err(|_| {
                Error::new(
                    500,
                    "credential_decryption_failed",
                    "Credential key does not match stored data",
                )
            })?;
        serde_json::from_slice(&plaintext).map_err(|_| Error::storage())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayKey {
    pub id: Uuid,
    pub group_id: Uuid,
    pub name: String,
    pub prefix: String,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSession {
    pub id: Uuid,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ciphertext_is_bound_to_account() {
        let cipher = CredentialCipher::new(&[7; 32]);
        let id = Uuid::new_v4();
        let c = Credentials {
            access_token: "secret".into(),
            refresh_token: "refresh".into(),
            id_token: "".into(),
            expires_at: None,
        };
        let encrypted = cipher.encrypt(id, &c).unwrap();
        assert_eq!(
            cipher.decrypt(id, &encrypted).unwrap().access_token,
            "secret"
        );
        assert!(cipher.decrypt(Uuid::new_v4(), &encrypted).is_err());
        assert!(!format!("{c:?}").contains("refresh"));
    }
}
