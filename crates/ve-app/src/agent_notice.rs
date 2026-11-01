//! The desktop notification for an agent's approval request: who asked for which Module, its
//! code, and both ways to approve it. Linux posts it to `org.freedesktop.Notifications`, macOS
//! through `osascript`; Windows has no notification here (the request still waits in Settings →
//! Permissions).

use ve_engine::agents::{CODE_LIFETIME, Request};

/// The notification's title and body for `request`.
fn text(request: &Request) -> (String, String) {
    let Request { code, agent, module, .. } = request;
    (
        "Agent access requested".to_owned(),
        format!(
            "The agent \u{201c}{agent}\u{201d} asks to read the {module} Module. To grant the Permission, approve code {code} \
             in Settings \u{2192} Permissions or run `ve mcp --approve {code}` within {} minutes.",
            CODE_LIFETIME / 60
        ),
    )
}

/// Shows the notification for `request` without waiting for the desktop.
pub fn show(request: &Request) {
    let (title, body) = text(request);
    let _ = std::thread::Builder::new().name("agent notice".into()).spawn(move || {
        if let Err(e) = post(&title, &body) {
            eprintln!("Could not show the agent's request on the desktop: {e}");
        }
    });
}

#[cfg(target_os = "linux")]
fn post(title: &str, body: &str) -> Result<(), String> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let connection = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                crate::app::APP_NAME,
                0u32,
                "view-everything",
                title,
                body,
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
fn post(title: &str, body: &str) -> Result<(), String> {
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let script = format!("display notification {} with title {}", quoted(body), quoted(title));
    let status = std::process::Command::new("osascript")
        .args(["-e", &script])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("osascript exited with {status}"))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn post(_title: &str, _body: &str) -> Result<(), String> {
    Ok(())
}
