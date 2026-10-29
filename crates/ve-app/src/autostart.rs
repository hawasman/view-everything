//! Start at login: an XDG autostart entry (Linux), a scheduled task that runs at logon with
//! highest privileges (Windows) or a LaunchAgent (macOS). The started app gets `--login`.

/// The flag a login start carries.
pub const LOGIN_FLAG: &str = "--login";

/// Whether this process was started by the login entry.
pub fn started_at_login() -> bool {
    std::env::args().any(|a| a == LOGIN_FLAG)
}

/// What a login start runs: the AppImage when running from one (its mount point is temporary),
/// else this executable.
fn program() -> Result<std::path::PathBuf, String> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|a| !a.is_empty()) {
        return Ok(appimage.into());
    }
    std::env::current_exe().map_err(|e| format!("Could not find the app's own path: {e}"))
}

/// Creates (`on`) or removes the login entry. May block (Windows waits for the elevation prompt).
pub fn set(on: bool) -> Result<(), String> {
    platform::set(on)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::PathBuf;

    fn entry_path() -> Result<PathBuf, String> {
        dirs::config_dir()
            .map(|d| d.join("autostart").join("view-everything.desktop"))
            .ok_or_else(|| "No configuration folder found".to_owned())
    }

    /// A program path as the Exec key of a desktop entry reads it: quoted, with the characters
    /// the Desktop Entry specification reserves escaped. Its string-value rule unescapes `\\`
    /// before the quoting rule applies, so each of those escapes carries a doubled backslash.
    pub(super) fn exec_quote(path: &str) -> String {
        let mut out = String::from("\"");
        for c in path.chars() {
            match c {
                '\\' => out.push_str("\\\\\\\\"),
                '"' | '`' | '$' => {
                    out.push_str("\\\\");
                    out.push(c);
                }
                '%' => out.push_str("%%"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    pub(super) fn entry(program: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName={}\nComment=Keeps the Index fresh from the system tray\nExec={} {}\nIcon=view-everything\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            crate::app::APP_NAME,
            exec_quote(program),
            super::LOGIN_FLAG
        )
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = entry_path()?;
        if !on {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("Could not remove {}: {e}", path.display())),
            };
        }
        let program = super::program()?;
        if program.to_string_lossy().chars().any(char::is_control) {
            return Err(format!("The program path has a control character: {}", program.display()));
        }
        let dir = path.parent().ok_or_else(|| "No autostart folder".to_owned())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        std::fs::write(&path, entry(&program.to_string_lossy())).map_err(|e| format!("Could not write {}: {e}", path.display()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_entry_runs_the_program_with_the_login_flag() {
            let text = entry("/home/me/My Apps/view-everything.AppImage");
            assert!(text.contains("Exec=\"/home/me/My Apps/view-everything.AppImage\" --login\n"));
        }

        #[test]
        fn reserved_characters_in_the_path_are_escaped() {
            assert_eq!(exec_quote("/a\"b$c\\d"), "\"/a\\\\\"b\\\\$c\\\\\\\\d\"");
        }
    }
}

#[cfg(windows)]
mod platform {
    const TASK: &str = "ViewEverything";

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    /// The task: at logon, highest privileges, and none of the defaults that would stop a tray
    /// app (not starting on battery, stopping when unplugged, the 72-hour time limit).
    pub(super) fn task_xml(program: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
<Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n\
<Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>\n\
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\
<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Enabled>true</Enabled></Settings>\n\
<Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>\n\
</Task>\n",
            xml_escape(program),
            super::LOGIN_FLAG
        )
    }

    /// One elevated `schtasks` call, so the user answers one prompt per toggle.
    pub fn set(on: bool) -> Result<(), String> {
        let tag = std::process::id();
        let dir = std::env::temp_dir();
        let log = dir.join(format!("view-everything-task-{tag}.log"));
        let xml = dir.join(format!("view-everything-task-{tag}.xml"));
        let args = if on {
            let program = super::program()?;
            // UTF-16 with a byte order mark, as `schtasks /XML` reads it.
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(task_xml(&program.to_string_lossy()).encode_utf16().flat_map(u16::to_le_bytes));
            std::fs::write(&xml, bytes).map_err(|e| format!("Could not write {}: {e}", xml.display()))?;
            format!("/Create /TN {TASK} /XML \"{}\" /F", xml.display())
        } else {
            format!("/Delete /TN {TASK} /F")
        };
        // `cmd` carries schtasks' own message to a file: an elevated process cannot be piped.
        let command = format!("/c schtasks {args} > \"{}\" 2>&1", log.display());
        // PowerShell's single-quoted strings take a quote by doubling it.
        let script = format!(
            "$p = Start-Process cmd -ArgumentList '{}' -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode",
            command.replace('\'', "''")
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
            .status();
        let message = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&xml);
        let status = status.map_err(|e| format!("Could not run PowerShell: {e}"))?;
        if status.success() {
            return Ok(());
        }
        // Removing a task that is not there is the state asked for.
        if !on {
            let gone = std::process::Command::new("schtasks")
                .args(["/Query", "/TN", TASK])
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(false);
            if gone {
                return Ok(());
            }
        }
        let message = message.trim();
        if message.is_empty() {
            Err("The scheduled task was not changed (administrator approval is needed)".to_owned())
        } else {
            Err(format!("The scheduled task was not changed: {message}"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_task_runs_on_battery_without_a_time_limit() {
            let xml = task_xml("C:\\Apps\\R&D\\view-everything.exe");
            assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
            assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
            assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
            assert!(xml.contains("<Command>C:\\Apps\\R&amp;D\\view-everything.exe</Command>"));
            assert!(xml.contains("<Arguments>--login</Arguments>"));
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    const LABEL: &str = "org.view-everything.login";

    fn plist_path() -> Result<PathBuf, String> {
        dirs::home_dir()
            .map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
            .ok_or_else(|| "No home folder found".to_owned())
    }

    fn xml(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = plist_path()?;
        if !on {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("Could not remove {}: {e}", path.display())),
            };
        }
        let program = super::program()?;
        let text = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>{}</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n</dict>\n</plist>\n",
            xml(&program.to_string_lossy()),
            super::LOGIN_FLAG
        );
        let dir = path.parent().ok_or_else(|| "No LaunchAgents folder".to_owned())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("Could not write {}: {e}", path.display()))
    }
}
