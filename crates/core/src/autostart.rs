//! Cross-platform "Launch on Startup" registration.

use anyhow::Result;

const APP_ID: &str = "io.movefreely.desktop";
#[cfg(target_os = "windows")]
const APP_NAME: &str = "MoveFreely";

pub fn set_launch_on_startup(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot locate home dir"))?;
        let agents_dir = home.join("Library/LaunchAgents");
        let plist_path = agents_dir.join(format!("{APP_ID}.plist"));
        if !enabled {
            if plist_path.exists() {
                std::fs::remove_file(plist_path)?;
            }
            return Ok(());
        }
        std::fs::create_dir_all(&agents_dir)?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{APP_ID}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_str}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
        );
        std::fs::write(plist_path, plist)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let cfg_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot locate config dir"))?
            .join("autostart");
        let desktop_path = cfg_dir.join("move-freely.desktop");
        if !enabled {
            if desktop_path.exists() {
                std::fs::remove_file(desktop_path)?;
            }
            return Ok(());
        }
        std::fs::create_dir_all(&cfg_dir)?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Move Freely\nExec=\"{exe_str}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(desktop_path, entry)?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use windows::core::HSTRING;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
            KEY_SET_VALUE, REG_SZ,
        };
        let subkey = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let name = HSTRING::from(APP_NAME);
        let mut hkey = HKEY::default();
        unsafe {
            let status = RegOpenKeyExW(HKEY_CURRENT_USER, &subkey, 0, KEY_SET_VALUE, &mut hkey);
            if status.is_err() {
                anyhow::bail!("failed opening Windows Run registry key: {status:?}");
            }
            let res = if enabled {
                let quoted = format!("\"{exe_str}\"");
                let wide: Vec<u16> = quoted.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes =
                    std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                RegSetValueExW(hkey, &name, 0, REG_SZ, Some(bytes))
            } else {
                RegDeleteValueW(hkey, &name)
            };
            let _ = RegCloseKey(hkey);
            if res.is_err() && enabled {
                anyhow::bail!("failed updating Windows Run registry key: {res:?}");
            }
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (enabled, exe_str);
        Ok(())
    }
}

pub fn is_launch_on_startup_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|h| h.join("Library/LaunchAgents").join(format!("{APP_ID}.plist")).exists())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        dirs::config_dir()
            .map(|c| c.join("autostart/move-freely.desktop").exists())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        false
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}
