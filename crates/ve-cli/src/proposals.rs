//! `ve proposals`: the person's answer to agents' Proposals. It lists them, applies one after
//! checking each row against the Index again and asking, or dismisses one. Proposals are held
//! by the running app; this asks it over its socket (see [`ve_engine::proposals`]).

use chrono::TimeZone;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use ve_engine::format;
use ve_engine::proposals::{Check, Proposal};
use ve_engine::socket::{self, Client};

pub const USAGE: &str = "\
Usage:
  ve proposals                 Agents' Proposals, newest first, and where each stands
  ve proposals --apply <ID>    Check a pending Proposal against the Index again, show what moves
                               to the Trash, ask, then move it
  ve proposals --dismiss <ID>  Dismiss a pending Proposal: nothing moves

An agent makes a Proposal with `ve mcp serve` (files.propose_trash): a list of Entries to move to
the Trash, with its reason. It changes nothing until you apply it here, and lapses unanswered after
24 hours. Applying leaves out each row that changed or vanished since it was proposed, and says so.
Proposals are held by the running app.";

pub fn main(args: &[String]) -> ExitCode {
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => list(&mut io::stdout().lock()),
        ["--apply", id] => id_of(id).and_then(|id| apply(id, &mut io::stdin().lock(), &mut io::stdout().lock())),
        ["--dismiss", id] => id_of(id).and_then(|id| dismiss(id, &mut io::stdout().lock())),
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("ve: unknown proposals command\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ve: {e}");
            ExitCode::from(2)
        }
    }
}

fn id_of(text: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("{text} is not a Proposal id (`ve proposals` lists them)"))
}

/// Connects to the running app, or says why not.
fn connect() -> Result<Client, String> {
    let path = socket::path().ok_or("cannot work out where the app socket is")?;
    Client::connect(&path).map_err(|e| {
        if e.is_not_running() {
            "the app is not running; Proposals are held by the running app".to_owned()
        } else {
            format!("cannot ask the app: {e}")
        }
    })
}

/// Calls `method` on the running app and reads its result as `T`.
fn call<T: serde::de::DeserializeOwned>(client: &mut Client, method: &str, params: Value) -> Result<T, String> {
    let result = client.call(method, params).map_err(|e| e.to_string())?;
    serde_json::from_value(result).map_err(|e| format!("unreadable answer from the app: {e}"))
}

/// Unix seconds as local time, e.g. `2026-11-13 10:05`.
fn time(at: i64) -> String {
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(|| at.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

/// "3 items", "1 item".
fn items(n: usize) -> String {
    format!("{} item{}", format::count(n as u64), if n == 1 { "" } else { "s" })
}

/// `path` for the terminal: a control character in a file name (a newline, an escape
/// sequence) is shown escaped, so a name cannot fake or hide lines of what will move.
fn shown(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn list(out: &mut impl Write) -> Result<(), String> {
    let proposals: Vec<Proposal> = call(&mut connect()?, "proposals.list", Value::Null)?;
    let write = |out: &mut dyn Write| -> io::Result<()> {
        if proposals.is_empty() {
            return writeln!(out, "No Proposals.");
        }
        writeln!(out, "ID\tSTATUS\tAGENT\tITEMS\tSIZE\tSUBMITTED\tREASON")?;
        for p in &proposals {
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                p.id,
                p.status.name(),
                p.agent,
                p.rows.len(),
                format::size(p.on_disk_size()),
                time(p.submitted_at),
                p.reason
            )?;
        }
        out.flush()
    };
    write(out).map_err(|e| e.to_string())
}

/// Checks Proposal `id` again, shows what applying it moves and leaves out, asks on `input`,
/// and applies it once the answer is yes.
fn apply(id: u64, input: &mut impl BufRead, out: &mut impl Write) -> Result<(), String> {
    let mut client = connect()?;
    let Check { proposal, keep, dropped } = call(&mut client, "proposals.check", json!({ "id": id }))?;
    let io = |e: io::Error| e.to_string();
    writeln!(out, "Proposal {id} from {}: {}", proposal.agent, proposal.reason).map_err(io)?;
    if !dropped.is_empty() {
        writeln!(out, "Left out, {}:", items(dropped.len())).map_err(io)?;
        for d in &dropped {
            writeln!(out, "  {}: {}", shown(&d.path), d.why.text()).map_err(io)?;
        }
    }
    let paths: Vec<PathBuf> = keep.iter().map(|r| r.path.clone()).collect();
    if keep.is_empty() {
        writeln!(out, "Nothing is left to move to the Trash.").map_err(io)?;
    } else {
        let total: u64 = keep.iter().map(|r| r.on_disk_size).sum();
        writeln!(out, "To move to the Trash, {}, {} on disk:", items(keep.len()), format::size(total)).map_err(io)?;
        for row in &keep {
            writeln!(out, "  {}  ({})", shown(&row.path), format::size(row.on_disk_size)).map_err(io)?;
        }
        writeln!(
            out,
            "They can be restored from the Trash, and keep using space until the Trash is emptied."
        )
        .map_err(io)?;
        write!(out, "Move {} to the Trash? [y/N] ", items(keep.len())).map_err(io)?;
        out.flush().map_err(io)?;
        let mut answer = String::new();
        input.read_line(&mut answer).map_err(io)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(out, "Nothing moved; Proposal {id} is still pending.").map_err(io)?;
            return Ok(());
        }
    }
    let applied: Proposal = call(&mut client, "proposals.apply", json!({ "id": id, "paths": paths }))?;
    let outcome = applied.outcome.unwrap_or_default();
    if !outcome.trashed.is_empty() {
        writeln!(out, "Moved {} to the Trash.", items(outcome.trashed.len())).map_err(io)?;
    }
    // Rows that changed between the check and the move.
    for d in outcome.dropped.iter().filter(|d| !dropped.contains(d)) {
        writeln!(out, "Left out {}: {}", shown(&d.path), d.why.text()).map_err(io)?;
    }
    for f in &outcome.failed {
        writeln!(out, "Could not move {}: {}", shown(&f.path), f.error).map_err(io)?;
    }
    if outcome.trashed.is_empty() {
        writeln!(out, "Proposal {id} is applied; nothing moved.").map_err(io)?;
    } else {
        writeln!(out, "Proposal {id} is applied.").map_err(io)?;
    }
    if outcome.failed.is_empty() {
        Ok(())
    } else {
        Err(format!("{} could not be moved", items(outcome.failed.len())))
    }
}

fn dismiss(id: u64, out: &mut impl Write) -> Result<(), String> {
    let dismissed: Proposal = call(&mut connect()?, "proposals.dismiss", json!({ "id": id }))?;
    writeln!(out, "Dismissed Proposal {id} from {}: nothing moved.", dismissed.agent).map_err(|e| e.to_string())
}
