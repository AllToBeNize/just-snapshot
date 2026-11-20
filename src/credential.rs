//! Secrets live in the Windows Credential Manager, never in a file. Each one
//! is a generic credential named `Snapshot/<name>`, visible to the user under
//! Control Panel › Credential Manager › Windows Credentials.

use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_NOT_FOUND},
    Security::Credentials::*,
};

use crate::wide;

fn target(name: &str) -> Vec<u16> {
    wide(&format!("Snapshot/{name}"))
}

pub fn save(name: &str, secret: &str) -> Result<(), String> {
    let mut target = target(name);
    let mut user = wide("Snapshot");
    let mut blob = secret.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: user.as_mut_ptr(),
        ..Default::default()
    };
    let written = unsafe { CredWriteW(&credential, 0) } != 0;
    let error = std::io::Error::last_os_error();
    blob.fill(0);
    if written {
        Ok(())
    } else {
        Err(format!("无法把密钥保存到 Windows 凭据管理器。\n{error}"))
    }
}

/// The stored secret, or `None` if there is none.
pub fn load(name: &str) -> Result<Option<String>, String> {
    let target = target(name);
    let mut credential: *mut CREDENTIALW = null_mut();
    unsafe {
        if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) == 0 {
            return if GetLastError() == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(format!(
                    "无法读取 Windows 凭据管理器里的密钥。\n{}",
                    std::io::Error::last_os_error()
                ))
            };
        }
        let blob = std::slice::from_raw_parts(
            (*credential).CredentialBlob,
            (*credential).CredentialBlobSize as usize,
        );
        let secret = String::from_utf8_lossy(blob).into_owned();
        CredFree(credential.cast());
        Ok(Some(secret))
    }
}

#[cfg(test)]
pub fn delete(name: &str) -> Result<(), String> {
    let target = target(name);
    unsafe {
        if CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) == 0 && GetLastError() != ERROR_NOT_FOUND {
            return Err(format!(
                "无法删除 Windows 凭据管理器里的密钥。\n{}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a logon session with a credential store: the desktop session,
    /// not an SSH one. Run with `<test binary> credential --ignored`.
    #[test]
    #[ignore]
    fn round_trips_and_deletes() {
        let name = "test/round-trip";
        save(name, "sk-测试-123").unwrap();
        assert_eq!(load(name).unwrap().as_deref(), Some("sk-测试-123"));
        delete(name).unwrap();
        assert_eq!(load(name).unwrap(), None);
        delete(name).unwrap();
    }
}
