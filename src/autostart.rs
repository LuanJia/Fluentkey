//! Per-user login startup; never writes HKLM or requires elevation.
use anyhow::{bail, ensure, Context, Result};
use std::{
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::*,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "FluentKey";

#[derive(Clone, Debug, PartialEq)]
struct Value {
    kind: u32,
    bytes: Vec<u8>,
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn checked(code: u32, action: &str) -> Result<()> {
    if code != ERROR_SUCCESS {
        bail!(
            "{action}：{}",
            std::io::Error::from_raw_os_error(code as i32)
        );
    }
    Ok(())
}
fn read(path: &str) -> Result<Option<Value>> {
    let path = wide(path);
    let name = wide(VALUE_NAME);
    let mut handle = null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    checked(status, "无法读取开机启动项")?;
    let key = Key(handle);
    let mut len = 0;
    let mut kind = 0;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut len,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    checked(status, "无法读取开机启动项")?;
    ensure!(len <= 64 * 1024, "开机启动项数据异常，已停止修改");
    let mut bytes = vec![0; len as usize];
    checked(
        unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                null(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut len,
            )
        },
        "无法读取开机启动项",
    )?;
    bytes.truncate(len as usize);
    Ok(Some(Value { kind, bytes }))
}
fn write(path: &str, value: Option<&Value>) -> Result<()> {
    let path = wide(path);
    let name = wide(VALUE_NAME);
    let mut handle = null_mut();
    let status = unsafe {
        if value.is_some() {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut handle,
                null_mut(),
            )
        } else {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut handle,
            )
        }
    };
    if status == ERROR_FILE_NOT_FOUND && value.is_none() {
        return Ok(());
    }
    checked(status, "无法修改开机启动项")?;
    let key = Key(handle);
    let status = unsafe {
        match value {
            Some(value) => RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                value.kind,
                value.bytes.as_ptr(),
                value.bytes.len() as u32,
            ),
            None => RegDeleteValueW(key.0, name.as_ptr()),
        }
    };
    if status == ERROR_FILE_NOT_FOUND && value.is_none() {
        return Ok(());
    }
    checked(status, "无法修改开机启动项")
}
fn command(executable: &Path) -> Result<Value> {
    ensure!(executable.is_absolute(), "开机启动需要程序的绝对路径");
    let path: Vec<u16> = executable.as_os_str().encode_wide().collect();
    ensure!(
        !path.contains(&0) && !path.contains(&34),
        "程序路径不适合注册开机启动"
    );
    // Run accepts a command line of at most 260 characters. Quote the entire path
    // even without spaces so a moved executable cannot become an unquoted command.
    ensure!(
        path.len() + 2 <= 260,
        "程序路径过长，请移到较短的目录后启用开机启动"
    );
    let units = std::iter::once(34).chain(path).chain([34, 0]);
    Ok(Value {
        kind: REG_SZ,
        bytes: units.flat_map(u16::to_le_bytes).collect(),
    })
}
pub fn is_enabled() -> Result<bool> {
    let expected = command(&std::env::current_exe().context("无法确定程序路径")?)?;
    Ok(read(RUN_KEY)?.as_ref() == Some(&expected))
}
/// Apply the registry change together with settings persistence. If persistence
/// fails, restore the exact old value (including its registry type).
pub fn save_with(enabled: bool, persist: impl FnOnce() -> Result<()>) -> Result<()> {
    let executable = std::env::current_exe().context("无法确定程序路径")?;
    save_at(RUN_KEY, enabled, &executable, persist)
}
fn save_at(
    path: &str,
    enabled: bool,
    executable: &Path,
    persist: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let before = read(path)?;
    let after = if enabled {
        Some(command(executable)?)
    } else {
        None
    };
    let changed = before != after;
    if changed {
        write(path, after.as_ref())?;
    }
    if let Err(error) = persist() {
        if changed {
            if let Err(rollback) = write(path, before.as_ref()) {
                bail!("保存设置失败：{error}；恢复开机启动项也失败：{rollback}。请重新打开设置检查状态");
            }
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quotes_executable_and_rejects_long_or_relative_paths() {
        let value = command(Path::new(r"C:\应用 程序\FluentKey.exe")).unwrap();
        let units: Vec<u16> = value
            .bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            "\"C:\\应用 程序\\FluentKey.exe\"\0"
        );
        assert!(command(Path::new("FluentKey.exe")).is_err());
        assert!(command(Path::new(&format!("C:\\{}.exe", "a".repeat(260)))).is_err());
    }
    #[test]
    fn registry_enable_disable_and_rollback_in_isolated_key() {
        // Never exercise the actual Run key in tests.
        let path = format!(r"Software\FluentKey\Tests\Autostart-{}", std::process::id());
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.0).as_ptr());
                }
            }
        }
        let _cleanup = Cleanup(path.clone());
        let exe = Path::new(r"C:\测试 文件夹\FluentKey.exe");
        assert_eq!(read(&path).unwrap(), None);
        save_at(&path, true, exe, || Ok(())).unwrap();
        let expected = Some(command(exe).unwrap());
        assert_eq!(read(&path).unwrap(), expected);
        assert!(save_at(&path, false, exe, || bail!("simulated config failure")).is_err());
        assert_eq!(read(&path).unwrap(), expected);
        save_at(&path, false, exe, || Ok(())).unwrap();
        assert_eq!(read(&path).unwrap(), None);
        assert!(save_at(&path, true, exe, || bail!("simulated config failure")).is_err());
        assert_eq!(read(&path).unwrap(), None);
        // Disabling an absent entry is idempotent.
        save_at(&path, false, exe, || Ok(())).unwrap();
    }
}
