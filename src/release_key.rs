//! Signatures on update manifests: ECDSA P-256 from Windows CNG, no extra
//! crates. Each machine that publishes releases has its own private key in
//! its Credential Manager (`just-snapshot/release-key/<id>`); it never leaves
//! that machine. The app trusts every public key in `TRUSTED_KEYS`, so adding
//! or retiring a publishing machine is a source change shipped in a release
//! signed by a machine that is already trusted.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Security::Cryptography::*;

use crate::{credential, digest};

/// `(id, base64 BCRYPT_ECCPUBLIC_BLOB)` of every machine allowed to publish.
/// Add one with `snapshot.ps1 release-key -KeyId <id>` on that machine.
const TRUSTED_KEYS: &[(&str, &str)] = &[(
    "inver-pc-13",
    "RUNTMSAAAACzZYnXsrKqwSPnb5LyMoTH3OKJGQwcWOB+FLfwhOSayXNwNYt2Zv872QDQGnYpLzUtkItdXFAYGWpTnWIM09I0",
)];

const SIGNATURE_LEN: usize = 64;

fn credential_name(id: &str) -> String {
    format!("release-key/{id}")
}

struct Key(BCRYPT_KEY_HANDLE);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { BCryptDestroyKey(self.0) };
    }
}

fn check(status: i32, step: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{step}失败（CNG 错误 {:#x}）", status as u32))
    }
}

fn import(blob_type: *const u16, blob: &[u8]) -> Result<Key, String> {
    let mut key = null_mut();
    let status = unsafe {
        BCryptImportKeyPair(
            BCRYPT_ECDSA_P256_ALG_HANDLE,
            null_mut(),
            blob_type,
            &mut key,
            blob.as_ptr(),
            blob.len() as u32,
            0,
        )
    };
    check(status, "读取密钥")?;
    Ok(Key(key))
}

fn export(key: &Key, blob_type: *const u16) -> Result<Vec<u8>, String> {
    let mut size = 0;
    check(
        unsafe { BCryptExportKey(key.0, null_mut(), blob_type, null_mut(), 0, &mut size, 0) },
        "导出密钥",
    )?;
    let mut blob = vec![0u8; size as usize];
    check(
        unsafe { BCryptExportKey(key.0, null_mut(), blob_type, blob.as_mut_ptr(), size, &mut size, 0) },
        "导出密钥",
    )?;
    blob.truncate(size as usize);
    Ok(blob)
}

/// A fresh key pair: `(private blob, public blob)`.
fn generate_pair() -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut key = null_mut();
    check(
        unsafe { BCryptGenerateKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, &mut key, 256, 0) },
        "生成密钥",
    )?;
    let key = Key(key);
    check(unsafe { BCryptFinalizeKeyPair(key.0, 0) }, "生成密钥")?;
    Ok((export(&key, BCRYPT_ECCPRIVATE_BLOB)?, export(&key, BCRYPT_ECCPUBLIC_BLOB)?))
}

fn sign_with(private: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
    let key = import(BCRYPT_ECCPRIVATE_BLOB, private)?;
    let hash = digest::sha256_bytes(message);
    let mut signature = vec![0u8; SIGNATURE_LEN];
    let mut size = 0;
    check(
        unsafe {
            BCryptSignHash(
                key.0,
                null(),
                hash.as_ptr(),
                hash.len() as u32,
                signature.as_mut_ptr(),
                SIGNATURE_LEN as u32,
                &mut size,
                0,
            )
        },
        "签名",
    )?;
    signature.truncate(size as usize);
    Ok(signature)
}

fn verify_with(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = import(BCRYPT_ECCPUBLIC_BLOB, public) else {
        return false;
    };
    let hash = digest::sha256_bytes(message);
    let status = unsafe {
        BCryptVerifySignature(
            key.0,
            null(),
            hash.as_ptr(),
            hash.len() as u32,
            signature.as_ptr(),
            signature.len() as u32,
            0,
        )
    };
    status == 0
}

/// Checks that `signature` (base64) over `message` comes from the trusted
/// key `id`.
pub fn verify(id: &str, message: &[u8], signature: &str) -> Result<(), String> {
    verify_among(TRUSTED_KEYS, id, message, signature)
}

fn verify_among(keys: &[(&str, &str)], id: &str, message: &[u8], signature: &str) -> Result<(), String> {
    let (_, public) = keys
        .iter()
        .find(|(known, _)| *known == id)
        .ok_or_else(|| format!("更新信息的签名来自不认识的密钥「{id}」"))?;
    let public = base64_decode(public).ok_or("内置的公钥格式不对")?;
    let signature = base64_decode(signature).ok_or("更新信息的签名格式不对")?;
    if verify_with(&public, message, &signature) {
        Ok(())
    } else {
        Err("更新信息的签名对不上，可能被篡改，已拒绝".into())
    }
}

/// Makes a key pair for this machine, keeps the private half in the
/// Credential Manager and returns the public half (base64) for
/// `TRUSTED_KEYS`. Refuses to replace an existing key.
pub fn create(id: &str) -> Result<String, String> {
    if credential::load(&credential_name(id))?.is_some() {
        return Err(format!("这台电脑已经有名为「{id}」的发布密钥，没有覆盖"));
    }
    let (mut private, public) = generate_pair()?;
    let saved = credential::save(&credential_name(id), &base64_encode(&private));
    private.fill(0);
    saved?;
    Ok(base64_encode(&public))
}

/// Signs `message` with this machine's key `id`; returns base64.
pub fn sign(id: &str, message: &[u8]) -> Result<String, String> {
    let stored = credential::load(&credential_name(id))?
        .ok_or_else(|| format!("这台电脑上没有名为「{id}」的发布密钥"))?;
    let mut private = base64_decode(&stored).ok_or("保存的发布密钥格式不对")?;
    let signature = sign_with(&private, message);
    private.fill(0);
    let signature = signature?;
    // Never publish a signature the app would not accept.
    let public = TRUSTED_KEYS
        .iter()
        .find(|(known, _)| *known == id)
        .ok_or_else(|| format!("密钥「{id}」还不在程序内置的可信公钥里，用它签名的更新不会被接受"))?;
    let encoded = base64_encode(&signature);
    verify_among(&[*public], id, message, &encoded)?;
    Ok(encoded)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, &byte)| {
            value | u32::from(byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(ALPHABET[(value >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let mut value = 0u32;
    let mut bits = 0;
    for character in text.bytes() {
        let digit = ALPHABET.iter().position(|&known| known == character)? as u32;
        value = value << 6 | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((value >> bits) as u8);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for length in 0..40 {
            let bytes: Vec<u8> = (0..length).map(|index| (index * 37 + 11) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert!(base64_decode("a$b").is_none());
    }

    #[test]
    fn only_a_trusted_key_and_an_untouched_message_verify() {
        let (private, public) = generate_pair().unwrap();
        let (_, stranger) = generate_pair().unwrap();
        let public = base64_encode(&public);
        let stranger = base64_encode(&stranger);
        let keys = [("pc", public.as_str()), ("old", stranger.as_str())];
        let message = br#"{"version":"9.9.9"}"#;
        let signature = base64_encode(&sign_with(&private, message).unwrap());
        assert!(verify_among(&keys, "pc", message, &signature).is_ok());
        assert!(verify_among(&keys, "pc", br#"{"version":"9.9.8"}"#, &signature).is_err());
        assert!(verify_among(&keys, "old", message, &signature).is_err());
        assert!(verify_among(&keys, "nobody", message, &signature).is_err());
    }
}
