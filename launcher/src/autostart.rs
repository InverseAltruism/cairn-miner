//! Windows "start on login" toggle, via the per-user Run key
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! Off by default (nothing is written until the user enables it). It registers
//! the *launcher* — so on login the themed UI opens — and does NOT auto-start
//! mining (the user's chosen behavior). On non-Windows this is a no-op; Linux/
//! HiveOS rigs autostart mining through systemd / `mine-auto.sh` instead.

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "CairnMinerLauncher";

/// Is login-autostart currently registered?
#[cfg(windows)]
pub fn is_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(VALUE_NAME))
        .is_ok()
}

/// Enable or disable login-autostart for `exe` (the launcher's own path).
#[cfg(windows)]
pub fn set(enabled: bool, exe: &std::path::Path) -> std::io::Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if enabled {
        // Quote the path so a `Program Files` space doesn't split the command.
        let cmd = format!("\"{}\"", exe.display());
        run.set_value(VALUE_NAME, &cmd)
    } else {
        match run.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            // Already absent is success.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set(_enabled: bool, _exe: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}
