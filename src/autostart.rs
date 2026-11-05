//! "Start with Windows".
//!
//! Enabling it writes a single value, `Topebag`, under
//! `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`, the
//! standard per-user startup list (visible in Task Manager → Startup apps).
//! It launches `topebag.exe gui --background`, which starts straight into the
//! system tray. Disabling it deletes that value again. Nothing else is
//! touched and no administrator rights are needed.

use std::path::Path;

use anyhow::Result;

/// Registry key of the per-user startup list.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Name of Topebag's value in that key.
pub const VALUE_NAME: &str = "Topebag";

/// The command line registered for startup.
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" gui --background", exe.display())
}

/// Is Topebag registered to start with Windows?
pub fn is_enabled() -> bool {
    platform::read(RUN_KEY, VALUE_NAME).is_some()
}

/// Register (or unregister) the current executable to start at sign-in.
pub fn set_enabled(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe()?;
        platform::write(RUN_KEY, VALUE_NAME, &command_line(&exe))
    } else {
        platform::delete(RUN_KEY, VALUE_NAME)
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Result, bail};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    #[cfg(test)]
    use windows::Win32::System::Registry::RegDeleteKeyW;
    use windows::core::{HSTRING, PCWSTR};

    pub fn read(key: &str, name: &str) -> Option<String> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let mut len = 0u32;
        // SAFETY: first call only queries the size.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, Some(&mut len))
        };
        if status != ERROR_SUCCESS || len == 0 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        // SAFETY: `buf` holds `len` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    pub fn write(key: &str, name: &str, value: &str) -> Result<()> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given byte size.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            bail!("could not update the startup list (error {})", status.0);
        }
        Ok(())
    }

    pub fn delete(key: &str, name: &str) -> Result<()> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        // SAFETY: plain registry call with valid NUL-terminated strings.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr())) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            bail!("could not update the startup list (error {})", status.0);
        }
        Ok(())
    }

    /// Remove an (empty) key; only used to clean up after tests.
    #[cfg(test)]
    pub fn delete_key(key: &str) {
        let key = HSTRING::from(key);
        // SAFETY: plain registry call with a valid NUL-terminated string.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()));
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Result, bail};

    pub fn read(_key: &str, _name: &str) -> Option<String> {
        None
    }

    pub fn write(_key: &str, _name: &str, _value: &str) -> Result<()> {
        bail!("starting with the system is only supported on Windows for now")
    }

    pub fn delete(_key: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_the_path() {
        let cmd = command_line(Path::new(r"C:\Program Files\Topebag\topebag.exe"));
        assert_eq!(cmd, r#""C:\Program Files\Topebag\topebag.exe" gui --background"#);
    }

    /// Exercises the registry helpers on a throwaway key (never the real
    /// startup list).
    #[cfg(windows)]
    #[test]
    fn registry_round_trip_on_a_scratch_key() {
        let key = r"Software\TopebagTest";
        let name = format!("t{}", std::process::id());
        platform::write(key, &name, "hello").unwrap();
        assert_eq!(platform::read(key, &name).as_deref(), Some("hello"));
        platform::delete(key, &name).unwrap();
        assert_eq!(platform::read(key, &name), None);
        platform::delete(key, &name).unwrap(); // deleting twice is fine
        platform::delete_key(key);
    }
}
