//! Argon2id key derivation + XChaCha20-Poly1305. The index (file names included)
//! and every block are sealed; a wrong password fails on the index tag.

use anyhow::{anyhow, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const NONCE_LEN: usize = 24;

#[derive(Clone, Copy, Debug)]
pub struct KdfParams {
    pub salt: [u8; 16],
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    pub fn fresh() -> KdfParams {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        KdfParams { salt, m_kib: 64 * 1024, t: 3, p: 1 }
    }
}

pub struct Crypto {
    cipher: XChaCha20Poly1305,
}

impl Crypto {
    pub fn derive(password: &str, k: &KdfParams) -> Result<Crypto> {
        // Refuse absurd costs from a crafted header before allocating.
        if k.m_kib > 4 * 1024 * 1024 || k.t > 64 || k.p > 64 {
            return Err(anyhow!("archive asks for unreasonable key-derivation cost"));
        }
        let params = Params::new(k.m_kib, k.t, k.p, Some(32)).map_err(|e| anyhow!("argon2 params: {e}"))?;
        let mut key = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(password.as_bytes(), &k.salt, &mut key)
            .map_err(|e| anyhow!("argon2: {e}"))?;
        Ok(Crypto { cipher: XChaCha20Poly1305::new((&key).into()) })
    }

    /// Returns nonce || ciphertext || tag.
    pub fn seal(&self, aad: &[u8], msg: &[u8]) -> Result<Vec<u8>> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = self.cipher.encrypt(&nonce, Payload { msg, aad }).map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN + 16 {
            return Err(anyhow!("sealed data too short"));
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        self.cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| anyhow!("authentication failed"))
    }
}

pub fn block_aad(id: u32) -> [u8; 5] {
    let b = id.to_le_bytes();
    [b'B', b[0], b[1], b[2], b[3]]
}

pub const INDEX_AAD: &[u8] = b"index";
