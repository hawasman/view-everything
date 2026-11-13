//! The desktop notifications of agents: an approval request (who asked to read or propose in
//! which Module, its code, and both ways to approve it) and a new Proposal (which agent, what it
//! proposes and why, and how to answer it). Linux posts them to `org.freedesktop.Notifications`,
//! macOS through `osascript`; Windows has no notification here (a request still waits in
//! Settings → Permissions, a Proposal in `ve proposals`).

use ve_engine::agents::{CODE_LIFETIME, Request};
use ve_engine::format;
use ve_engine::proposals::{Change, Proposal};

/// The notification's title and body for `request`.
fn text(request: &Request) -> (String, String) {
    let Request { code, agent, .. } = request;
    (
        "Agent access requested".to_owned(),
        format!(
            "The agent \u{201c}{agent}\u{201d} asks to {}. To grant the Permission, approve code {code} \
             in Settings \u{2192} Permissions or run `ve mcp --approve {code}` within {} minutes.",
            request.what(),
            CODE_LIFETIME / 60
        ),
    )
}

/// The notification's title and body for `proposal`.
fn proposal_text(proposal: &Proposal) -> (String, String) {
    let Proposal { id, agent, reason, .. } = proposal;
    let what = match proposal.change {
        Change::Trash => format!(
            "moving {} item{} ({}) to the Trash",
            format::count(proposal.rows.len() as u64),
            if proposal.rows.len() == 1 { "" } else { "s" },
            format::size(proposal.on_disk_size())
        ),
    };
    let hours = (proposal.lapses_at - proposal.submitted_at) / 3600;
    (
        "Agent Proposal".to_owned(),
        format!(
            "The agent \u{201c}{agent}\u{201d} proposes {what}: {reason}. Nothing has moved. Review and apply it with \
             `ve proposals --apply {id}`, or dismiss it with `ve proposals --dismiss {id}`; unanswered, it lapses in {hours} hours."
        ),
    )
}

/// Shows the notification for `request` without waiting for the desktop.
pub fn show(request: &Request) {
    notify(text(request));
}

/// Shows the notification for `proposal` without waiting for the desktop.
pub fn show_proposal(proposal: &Proposal) {
    notify(proposal_text(proposal));
}

fn notify((title, body): (String, String)) {
    let _ = std::thread::Builder::new().name("agent notice".into()).spawn(move || {
        if let Err(e) = post(&title, &body) {
            eprintln!("Could not show the agent's notification on the desktop: {e}");
        }
    });
}

#[cfg(target_os = "linux")]
fn post(title: &str, body: &str) -> Result<(), String> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    // Servers that support body markup read `<`, `>` and `&` as markup; the agent writes its
    // name and a Proposal's reason.
    let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let connection = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    // Ties the notification to the app's desktop file, so the desktop shows its name and icon.
    let hints = HashMap::from([("desktop-entry", Value::from(crate::app::APP_ID))]);
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
                body.as_str(),
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
