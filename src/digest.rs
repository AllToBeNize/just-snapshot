//! MD5 and SHA-256 from Windows CNG, for request signatures and update checks.

use windows_sys::Win32::Security::Cryptography::{
    BCryptHash, BCRYPT_ALG_HANDLE, BCRYPT_MD5_ALG_HANDLE, BCRYPT_SHA256_ALG_HANDLE,
};

fn hash<const N: usize>(algorithm: BCRYPT_ALG_HANDLE, input: &[u8]) -> String {
    let mut output = [0u8; N];
    let status = unsafe {
        BCryptHash(
            algorithm,
            std::ptr::null(),
            0,
            input.as_ptr(),
            input.len() as u32,
            output.as_mut_ptr(),
            N as u32,
        )
    };
    assert_eq!(status, 0, "BCryptHash failed: {status:#x}");
    output.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn md5(input: &str) -> String {
    hash::<16>(BCRYPT_MD5_ALG_HANDLE, input.as_bytes())
}

pub fn sha256(input: &str) -> String {
    hash::<32>(BCRYPT_SHA256_ALG_HANDLE, input.as_bytes())
}

/// SHA-256 of raw bytes, as bytes.
pub fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut output = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            input.as_ptr(),
            input.len() as u32,
            output.as_mut_ptr(),
            32,
        )
    };
    assert_eq!(status, 0, "BCryptHash failed: {status:#x}");
    output
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_digests() {
        assert_eq!(md5(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
