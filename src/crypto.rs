use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use zeroize::Zeroize;

use crate::error::{Error, InternalError};

const MAGIC: &[u8; 5] = b"MYPH1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;

/// Data-folder encryption key. Zeroized on drop.
#[derive(Clone, zeroize::ZeroizeOnDrop)]
pub struct DataKey(pub [u8; 32]);

/// Derive a 32-byte key from passphrase + salt (Argon2id).
pub fn derive_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<DataKey, Error> {
    let argon = Argon2::default();
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|_| Error::Internal(InternalError::Crypto))?;
    Ok(DataKey(key))
}

/// Encrypt plaintext. Output: MAGIC | salt | nonce | ciphertext.
pub fn encrypt(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt)?;
    encrypt_with_key(&key, &salt, plaintext)
}

pub fn encrypt_with_key(
    key: &DataKey,
    salt: &[u8; SALT_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key.0));
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| Error::Internal(InternalError::Crypto))?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub struct Decrypted {
    pub key: DataKey,
    pub salt: [u8; SALT_LEN],
    pub plaintext: Vec<u8>,
}

impl Drop for Decrypted {
    fn drop(&mut self) {
        self.plaintext.zeroize();
    }
}

/// Decrypt a MYPH1 blob using the passphrase.
pub fn decrypt(passphrase: &str, blob: &[u8]) -> Result<Decrypted, Error> {
    let min = MAGIC.len() + SALT_LEN + NONCE_LEN + 16;
    if blob.len() < min || &blob[..MAGIC.len()] != MAGIC {
        return Err(Error::user(
            "Data folder is not a Myphin ledger, or the file is corrupt.",
        ));
    }
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&blob[MAGIC.len()..MAGIC.len() + SALT_LEN]);
    let nonce_start = MAGIC.len() + SALT_LEN;
    let nonce_bytes = &blob[nonce_start..nonce_start + NONCE_LEN];
    let ciphertext = &blob[nonce_start + NONCE_LEN..];
    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key.0));
    let nonce = XNonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| Error::user("Wrong passphrase, or the ledger is corrupt."))?;
    Ok(Decrypted {
        key,
        salt,
        plaintext,
    })
}

/// Re-encrypt with an existing key and salt (same passphrase).
pub fn encrypt_existing(
    key: &DataKey,
    salt: &[u8; SALT_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    encrypt_with_key(key, salt, plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let blob = encrypt("correct horse", b"hello ledger").unwrap();
        let dec = decrypt("correct horse", &blob).unwrap();
        assert_eq!(dec.plaintext, b"hello ledger");
        assert!(decrypt("wrong", &blob).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decrypt("x", b"nope").is_err());
    }
}
