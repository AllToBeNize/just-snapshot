//! Secrets live in the Windows Credential Manager, never in a file. Each one
//! is a generic credential named `just-snapshot/<name>`, visible to the user under
//! Control Panel › Credential Manager › Windows Credentials.

use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_NOT_FOUND},
    Security::Credentials::*,
};

use crate::wide;

fn target(name: &str) -> Vec<u16> {
    wide(&format!("{}/{name}", crate::migrate::NAME))
}

pub fn save(name: &str, secret: &str) -> Result<(), String> {
    write(target(name), secret.as_bytes().to_vec())
}

fn write(mut target: Vec<u16>, mut blob: Vec<u8>) -> Result<(), String> {
    let mut user = wide(crate::migrate::NAME);
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

/// Moves every `<old>/…` secret to `<new>/…` (one already under the new
/// name is kept and the old one left alone); returns what could not move.
pub fn migrate(old: &str, new: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let filter = wide(&format!("{old}/*"));
    let mut count = 0;
    let mut list: *mut *mut CREDENTIALW = null_mut();
    unsafe {
        if CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut list) == 0 {
            if GetLastError() != ERROR_NOT_FOUND {
                failures.push(format!("无法列出旧的密钥：{}", std::io::Error::last_os_error()));
            }
            return failures;
        }
        for &credential in std::slice::from_raw_parts(list, count as usize) {
            let credential = &*credential;
            if credential.Type != CRED_TYPE_GENERIC {
                continue;
            }
            let length = (0..).take_while(|&index| *credential.TargetName.add(index) != 0).count();
            let old_target = String::from_utf16_lossy(std::slice::from_raw_parts(credential.TargetName, length));
            let Some(rest) = old_target.strip_prefix(&format!("{old}/")) else { continue };
            let new_target = wide(&format!("{new}/{rest}"));
            let mut existing: *mut CREDENTIALW = null_mut();
            if CredReadW(new_target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut existing) != 0 {
                CredFree(existing.cast());
                continue;
            }
            let blob = std::slice::from_raw_parts(credential.CredentialBlob, credential.CredentialBlobSize as usize).to_vec();
            match write(new_target, blob) {
                Ok(()) => {
                    CredDeleteW(credential.TargetName, CRED_TYPE_GENERIC, 0);
                }
                Err(error) => failures.push(format!("密钥 {old_target}：{error}")),
            }
        }
        CredFree(list.cast());
    }
    failures
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

/// Deletes every secret under `just-snapshot/<prefix>/`, for uninstalling;
/// returns what could not be deleted.
pub fn delete_all(prefix: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let filter = target(&format!("{prefix}/*"));
    let mut count = 0;
    let mut list: *mut *mut CREDENTIALW = null_mut();
    unsafe {
        if CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut list) == 0 {
            if GetLastError() != ERROR_NOT_FOUND {
                failures.push(format!("无法列出保存的密钥：{}", std::io::Error::last_os_error()));
            }
            return failures;
        }
        for &credential in std::slice::from_raw_parts(list, count as usize) {
            let credential = &*credential;
            if credential.Type == CRED_TYPE_GENERIC
                && CredDeleteW(credential.TargetName, CRED_TYPE_GENERIC, 0) == 0
                && GetLastError() != ERROR_NOT_FOUND
            {
                let length = (0..).take_while(|&index| *credential.TargetName.add(index) != 0).count();
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(credential.TargetName, length));
                failures.push(format!("密钥 {name}：{}", std::io::Error::last_os_error()));
            }
        }
        CredFree(list.cast());
    }
    failures
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

    /// Same session needs as above. Run with `<test binary> credential --ignored`.
    #[test]
    #[ignore]
    fn migrates_secrets_to_the_new_prefix() {
        let old = "just-snapshot-test-old";
        let old_target = wide(&format!("{old}/test/migrate"));
        write(old_target.clone(), "旧密钥".as_bytes().to_vec()).unwrap();
        assert!(migrate(old, crate::migrate::NAME).is_empty());
        assert_eq!(load("test/migrate").unwrap().as_deref(), Some("旧密钥"));
        let mut gone: *mut CREDENTIALW = null_mut();
        assert_eq!(unsafe { CredReadW(old_target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut gone) }, 0, "the old one is removed");
        delete("test/migrate").unwrap();
    }
}
