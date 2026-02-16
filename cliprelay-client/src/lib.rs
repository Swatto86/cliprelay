#[cfg(target_os = "windows")]
pub mod autostart {
    use std::{fmt, path::Path, string::FromUtf16Error};

    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_SZ,
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    #[derive(Debug)]
    pub enum AutostartError {
        RegOpenRunKey { status: u32 },
        RegQuerySize { status: u32 },
        RegQueryData { status: u32 },
        RegSet { status: u32 },
        RegDelete { status: u32 },
        ValueTooLarge,
        InvalidUtf16(FromUtf16Error),
    }

    impl fmt::Display for AutostartError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                AutostartError::RegOpenRunKey { status } => {
                    write!(f, "RegOpenKeyExW(Run) failed: {status}")
                }
                AutostartError::RegQuerySize { status } => {
                    write!(f, "RegQueryValueExW(size) failed: {status}")
                }
                AutostartError::RegQueryData { status } => {
                    write!(f, "RegQueryValueExW(data) failed: {status}")
                }
                AutostartError::RegSet { status } => write!(f, "RegSetValueExW failed: {status}"),
                AutostartError::RegDelete { status } => {
                    write!(f, "RegDeleteValueW failed: {status}")
                }
                AutostartError::ValueTooLarge => write!(f, "registry value too large"),
                AutostartError::InvalidUtf16(e) => write!(f, "invalid UTF-16 in Run value: {e}"),
            }
        }
    }

    impl std::error::Error for AutostartError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                AutostartError::InvalidUtf16(e) => Some(e),
                _ => None,
            }
        }
    }

    pub fn autostart_command(exe: &Path) -> String {
        format!("\"{}\" --background", exe.display())
    }

    pub fn is_enabled(exe: &Path, value_name: &str) -> Result<bool, AutostartError> {
        let expected = autostart_command(exe);
        Ok(run_key_get_value_string(value_name)?.is_some_and(|v| v.trim() == expected.trim()))
    }

    pub fn set_enabled(exe: &Path, value_name: &str, enabled: bool) -> Result<(), AutostartError> {
        if enabled {
            let cmd = autostart_command(exe);
            run_key_set_value_string(value_name, &cmd)?;
        } else {
            run_key_delete_value(value_name)?;
        }
        Ok(())
    }

    fn run_key_open(desired_access: u32) -> Result<HKEY, AutostartError> {
        let subkey = wide_null(RUN_SUBKEY);
        let mut out: HKEY = 0;
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                desired_access,
                &mut out,
            )
        };
        if status != 0 {
            return Err(AutostartError::RegOpenRunKey { status });
        }
        Ok(out)
    }

    fn run_key_get_value_string(name: &str) -> Result<Option<String>, AutostartError> {
        let key = run_key_open(KEY_READ | KEY_QUERY_VALUE)?;
        let name_w = wide_null(name);

        let mut value_type: u32 = 0;
        let mut size_bytes: u32 = 0;
        let status = unsafe {
            RegQueryValueExW(
                key,
                name_w.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type,
                std::ptr::null_mut(),
                &mut size_bytes,
            )
        };

        if status == ERROR_FILE_NOT_FOUND {
            unsafe { RegCloseKey(key) };
            return Ok(None);
        }

        if status != 0 {
            unsafe { RegCloseKey(key) };
            return Err(AutostartError::RegQuerySize { status });
        }

        if value_type != REG_SZ {
            unsafe { RegCloseKey(key) };
            return Ok(None);
        }

        if size_bytes == 0 {
            unsafe { RegCloseKey(key) };
            return Ok(Some(String::new()));
        }

        let mut buf: Vec<u8> = vec![0u8; size_bytes as usize];
        let mut size_bytes_2 = size_bytes;
        let status = unsafe {
            RegQueryValueExW(
                key,
                name_w.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type,
                buf.as_mut_ptr(),
                &mut size_bytes_2,
            )
        };
        unsafe { RegCloseKey(key) };
        if status != 0 {
            return Err(AutostartError::RegQueryData { status });
        }

        if buf.len() % 2 != 0 {
            return Ok(None);
        }
        let mut utf16: Vec<u16> = Vec::with_capacity(buf.len() / 2);
        for chunk in buf.chunks_exact(2) {
            utf16.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        if let Some(0) = utf16.last().copied() {
            utf16.pop();
        }

        String::from_utf16(&utf16)
            .map(Some)
            .map_err(AutostartError::InvalidUtf16)
    }

    fn run_key_set_value_string(name: &str, value: &str) -> Result<(), AutostartError> {
        let key = run_key_open(KEY_WRITE | KEY_SET_VALUE)?;
        let name_w = wide_null(name);
        let value_w = wide_null(value);
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2) };

        let status = unsafe {
            RegSetValueExW(
                key,
                name_w.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                u32::try_from(bytes.len()).map_err(|_| AutostartError::ValueTooLarge)?,
            )
        };
        unsafe { RegCloseKey(key) };
        if status != 0 {
            return Err(AutostartError::RegSet { status });
        }
        Ok(())
    }

    fn run_key_delete_value(name: &str) -> Result<(), AutostartError> {
        let key = run_key_open(KEY_WRITE | KEY_SET_VALUE)?;
        let name_w = wide_null(name);
        let status = unsafe { RegDeleteValueW(key, name_w.as_ptr()) };
        unsafe { RegCloseKey(key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if status != 0 {
            return Err(AutostartError::RegDelete { status });
        }
        Ok(())
    }

    fn wide_null(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn autostart_command_quotes_and_background() {
            let p = std::path::PathBuf::from(r"C:\Program Files\ClipRelay\cliprelay-client.exe");
            let cmd = autostart_command(&p);
            assert!(cmd.starts_with('"'));
            assert!(cmd.contains("\" --background"));
        }

        #[test]
        fn wide_null_has_trailing_nul() {
            let v = wide_null("abc");
            assert_eq!(v.last().copied(), Some(0));
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub mod autostart {
    use std::path::Path;

    #[derive(Debug)]
    pub struct AutostartError;

    impl std::fmt::Display for AutostartError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "autostart is only supported on Windows")
        }
    }

    impl std::error::Error for AutostartError {}

    pub fn autostart_command(exe: &Path) -> String {
        format!("\"{}\" --background", exe.display())
    }

    pub fn is_enabled(_exe: &Path, _value_name: &str) -> Result<bool, AutostartError> {
        Ok(false)
    }

    pub fn set_enabled(
        _exe: &Path,
        _value_name: &str,
        _enabled: bool,
    ) -> Result<(), AutostartError> {
        Ok(())
    }
}
