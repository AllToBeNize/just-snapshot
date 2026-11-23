use std::{
    env, io,
    mem::{size_of, size_of_val},
    os::windows::ffi::OsStrExt,
    ptr::null_mut,
};

use windows_sys::{
    core::w,
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
            RRF_RT_REG_SZ,
        },
    },
};

const RUN_KEY: *const u16 = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: *const u16 = w!("just-snapshot");
/// The value before the rename.
const OLD_VALUE_NAME: *const u16 = w!("Snapshot");
const MAX_COMMAND_LENGTH: usize = 260;

pub fn is_enabled() -> Result<bool, String> {
    read_command().map(|command| command.is_some())
}

pub fn save_with(
    enabled: bool,
    save_preferences: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let previous = read_command()?;
    let next = enabled.then(current_command).transpose()?;
    if previous == next {
        return save_preferences();
    }

    write_command(next.as_deref())?;
    if let Err(error) = save_preferences() {
        return match write_command(previous.as_deref()) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}\n无法恢复原自启动设置。\n{rollback}")),
        };
    }
    Ok(())
}

/// Points an existing start-at-login entry at this executable when the
/// program was moved or renamed (snapshot.exe became just-snapshot.exe).
pub fn follow_executable() -> Result<(), String> {
    if read_value(OLD_VALUE_NAME)?.is_some() {
        // Moved to the new value name, at this executable.
        if read_command()?.is_none() {
            write_command(Some(&current_command()?))?;
        }
        write_value(OLD_VALUE_NAME, None)?;
    }
    let Some(previous) = read_command()? else {
        return Ok(());
    };
    let current = current_command()?;
    if previous != current {
        write_command(Some(&current))?;
    }
    Ok(())
}

fn current_command() -> Result<Vec<u16>, String> {
    let executable = env::current_exe().map_err(|error| format!("无法获取程序路径。\n{error}"))?;
    let command: Vec<_> = std::iter::once(u16::from(b'"'))
        .chain(executable.as_os_str().encode_wide())
        .chain([u16::from(b'"'), 0])
        .collect();
    if command.len() > MAX_COMMAND_LENGTH + 1 {
        return Err("程序路径太长，无法设置登录时自动启动。".into());
    }
    Ok(command)
}

fn read_command() -> Result<Option<Vec<u16>>, String> {
    read_value(VALUE_NAME)
}

fn read_value(name: *const u16) -> Result<Option<Vec<u16>>, String> {
    let mut command = [0u16; MAX_COMMAND_LENGTH + 1];
    let mut bytes = size_of_val(&command) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            name,
            RRF_RT_REG_SZ,
            null_mut(),
            command.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    match status {
        ERROR_SUCCESS => Ok(Some(command[..bytes as usize / size_of::<u16>()].to_vec())),
        ERROR_FILE_NOT_FOUND => Ok(None),
        _ => Err(format!(
            "无法读取自启动设置。\n{}",
            io::Error::from_raw_os_error(status as i32)
        )),
    }
}

fn write_command(command: Option<&[u16]>) -> Result<(), String> {
    write_value(VALUE_NAME, command)
}

fn write_value(name: *const u16, command: Option<&[u16]>) -> Result<(), String> {
    let status = unsafe {
        match command {
            Some(command) => RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                name,
                REG_SZ,
                command.as_ptr().cast(),
                size_of_val(command) as u32,
            ),
            None => match RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, name) {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                status => status,
            },
        }
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "无法保存自启动设置。\n{}",
            io::Error::from_raw_os_error(status as i32)
        ));
    }
    Ok(())
}
