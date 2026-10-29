//! Cleanup Rules: the format shared by the built-in set (`builtin.toml`) and the user's
//! `cleanup.toml`, finding Cleanables from the Index, checking them again on disk and removing
//! them (ADR-0008).

use crate::trash::volume_blocker;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use ve_core::category::extension;
use ve_core::{Candidate, CategoryMap, EntryId, Index, Query, QueryContext, Totals};
use ve_scan::VolumeInfo;

/// What a `CACHEDIR.TAG` must start with (https://bford.info/cachedir/).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

const BUILTIN: &str = include_str!("builtin.toml");

/// How much of a command's output is kept for the report.
const OUTPUT_LIMIT: usize = 4000;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Temp,
    Cache,
    Build,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Temp, Group::Cache, Group::Build];

    pub fn title(self) -> &'static str {
        match self {
            Group::Temp => "Temporary Files",
            Group::Cache => "Caches",
            Group::Build => "Build Artefacts",
        }
    }
}

/// One Cleanup Rule as written in a rules file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    name: String,
    group: Group,
    #[serde(default)]
    platforms: Vec<String>,
    query: String,
    #[serde(default)]
    markers: Vec<String>,
    #[serde(default)]
    files: bool,
    #[serde(default = "yes")]
    tick: bool,
    #[serde(default)]
    command: Vec<String>,
    estimate: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    rule: Vec<Raw>,
}

/// A checked Cleanup Rule.
#[derive(Debug)]
pub struct Rule {
    pub name: String,
    pub group: Group,
    query: Query,
    markers: Vec<String>,
    files: bool,
    /// Pre-ticked when old enough (never for caches).
    tick: bool,
    /// Removes by running this argv in the matched folder instead of removing the matched paths.
    pub command: Vec<String>,
    estimate: Option<String>,
}

impl Rule {
    pub fn is_command(&self) -> bool {
        !self.command.is_empty()
    }

    /// The exact command, as shown in the advanced view.
    pub fn argv_text(&self) -> String {
        self.command
            .iter()
            .map(|a| {
                if a.is_empty() || a.contains([' ', '"', '\'']) {
                    format!("{a:?}")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The rules in force, and what was wrong with the ones that are not.
#[derive(Default)]
pub struct Rules {
    pub rules: Vec<Arc<Rule>>,
    /// Each as `file line N: reason`.
    pub errors: Vec<String>,
}

/// The built-in rules, then the ones in the user's `cleanup.toml` (when there is one).
pub fn load(user_file: Option<&Path>) -> Rules {
    let context = QueryContext::new(CategoryMap::builtin(), chrono::Local::now());
    let mut rules = parse(BUILTIN, "built-in rules", std::env::consts::OS, &context);
    if let Some(path) = user_file {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let user = parse(&text, "cleanup.toml", std::env::consts::OS, &context);
                rules.rules.extend(user.rules);
                rules.errors.extend(user.errors);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => rules.errors.push(format!("{}: {e}", path.display())),
        }
    }
    rules
}

/// Parses a rules file; rules for other platforms are dropped. A syntax error loses the whole
/// file, a bad rule only itself.
pub fn parse(text: &str, source: &str, os: &str, context: &QueryContext) -> Rules {
    let mut out = Rules::default();
    let file: File = match toml::from_str(text) {
        Ok(file) => file,
        Err(e) => {
            let line = e.span().map_or(0, |s| text[..s.start.min(text.len())].matches('\n').count() + 1);
            out.errors.push(format!("{source} line {line}: {}", e.message()));
            return out;
        }
    };
    // Where each [[rule]] starts, for reporting.
    let headers: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with("[[rule]]"))
        .map(|(i, _)| i + 1)
        .collect();
    for (n, raw) in file.rule.into_iter().enumerate() {
        match check(raw, os, context) {
            Ok(Some(rule)) => out.rules.push(Arc::new(rule)),
            Ok(None) => {}
            Err(why) => out.errors.push(format!(
                "{source} line {}: rule {}: {why}",
                headers.get(n).copied().unwrap_or(0),
                n + 1
            )),
        }
    }
    out
}

fn check(raw: Raw, os: &str, context: &QueryContext) -> Result<Option<Rule>, String> {
    let name = raw.name.trim().to_owned();
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if let Some(p) = raw.platforms.iter().find(|p| !["linux", "macos", "windows"].contains(&p.as_str())) {
        return Err(format!("`{name}`: unknown platform `{p}`; expected linux, macos or windows"));
    }
    if !(raw.platforms.is_empty() || raw.platforms.iter().any(|p| p == os)) {
        return Ok(None);
    }
    let text = expand(&raw.query).map_err(|e| format!("`{name}`: {e}"))?;
    let query = Query::parse(&text, context).map_err(|e| format!("`{name}`: query `{}`: {e}", raw.query))?;
    if raw.markers.is_empty() && !text.contains("parent:") {
        return Err(format!("`{name}`: a rule needs `markers` or a `parent:` location in its query"));
    }
    if query.needs_depth() {
        return Err(format!("`{name}`: depth: is not supported in a Cleanup Rule"));
    }
    for m in &raw.markers {
        let p = Path::new(m);
        if m.is_empty() || p.is_absolute() || !p.components().all(|c| matches!(c, Component::Normal(_) | Component::ParentDir)) {
            return Err(format!(
                "`{name}`: marker `{m}` must be a path relative to the match, such as `../Cargo.toml`"
            ));
        }
    }
    if raw.command.iter().any(String::is_empty) {
        return Err(format!("`{name}`: command has an empty argument"));
    }
    if raw.command.is_empty() && raw.estimate.is_some() {
        return Err(format!("`{name}`: estimate only applies to a command"));
    }
    Ok(Some(Rule {
        name,
        group: raw.group,
        query,
        markers: raw.markers,
        files: raw.files && raw.command.is_empty(),
        tick: raw.tick,
        command: raw.command,
        estimate: raw.estimate,
    }))
}

/// Fills in `{home}`, `{cache}` and `{temp}`.
fn expand(text: &str) -> Result<String, String> {
    let mut out = text.to_owned();
    for (key, dir) in [
        ("{home}", dirs::home_dir()),
        ("{cache}", dirs::cache_dir()),
        ("{temp}", Some(std::env::temp_dir())),
    ] {
        if out.contains(key) {
            let dir = dir.ok_or_else(|| format!("this system has no {key} folder"))?;
            out = out.replace(key, dir.to_string_lossy().replace('\\', "/").trim_end_matches('/'));
        }
    }
    Ok(out)
}

/// One thing a rule found.
#[derive(Debug, Clone)]
pub struct Cleanable {
    pub rule: Arc<Rule>,
    /// The path to remove, or the working folder of the command.
    pub path: PathBuf,
    /// On-disk Size; for a command rule only an estimate.
    pub on_disk: u64,
    /// Files under it by lower-cased extension (empty: none), for the Category breakdown.
    pub exts: Vec<(String, Totals)>,
    /// Modification time of the newest file in the project, in nanoseconds since the Unix epoch.
    pub newest: i64,
}

impl Cleanable {
    pub fn key(&self) -> (String, PathBuf) {
        (self.rule.name.clone(), self.path.clone())
    }

    /// The age limit that pre-ticks this: none for caches and for rules that say never.
    pub fn age_limit(&self, temp_days: u64, build_days: u64) -> Option<u64> {
        match self.rule.group {
            Group::Cache => None,
            _ if !self.rule.tick => None,
            Group::Temp => Some(temp_days),
            Group::Build => Some(build_days),
        }
    }

    /// Whether the Cleanup Plan starts with this ticked: not for caches, not where the rule says
    /// never, and only when nothing in the project changed within the age limit of its group.
    pub fn pre_ticked(&self, now_ns: i64, temp_days: u64, build_days: u64) -> bool {
        // No file at all (an unknown age) is not "old".
        self.age_limit(temp_days, build_days)
            .is_some_and(|days| self.newest > 0 && now_ns.saturating_sub(self.newest) > (days as i64).saturating_mul(NANOS_PER_DAY))
    }
}

fn newest_file(index: &Index, id: EntryId) -> i64 {
    let (mut newest, mut stack) = (0, vec![id]);
    while let Some(current) = stack.pop() {
        if index.is_dir(current) {
            stack.extend_from_slice(index.children(current));
        } else {
            newest = newest.max(index.modified(current));
        }
    }
    newest
}

/// Whether one of the rule's markers (or none are asked for) is a file in the Index.
fn marker_in_index(index: &Index, rule: &Rule, id: EntryId) -> bool {
    rule.markers.is_empty()
        || rule.markers.iter().any(|m| {
            let mut at = Some(id);
            for c in Path::new(m).components() {
                at = match (at, c) {
                    (Some(a), Component::ParentDir) => index.parent(a),
                    (Some(a), Component::Normal(n)) => n.to_str().and_then(|n| index.child_named(a, n)),
                    _ => None,
                };
            }
            at.is_some_and(|a| !index.is_dir(a))
        })
}

/// What `find` is doing, as its progress callback reports it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Running the rules over every Entry; progress counts Entries.
    Scanning,
    /// Measuring what was found; progress counts Cleanables.
    Sizing,
}

/// What the rules found in the Index, on Locations that can be changed, grouped in `Group::ALL`
/// order and largest first. A path inside another path that is removed whole is left out: the
/// outer one takes it along. A raised `cancel` ends the look early with nothing. `progress`
/// receives the phase, how many are done and how many there are, now and then.
pub fn find(
    index: &Index,
    volumes: &[VolumeInfo],
    rules: &[Arc<Rule>],
    cancel: &AtomicBool,
    progress: &dyn Fn(Phase, u64, u64),
) -> Vec<Cleanable> {
    let total = index.capacity() as u64;
    let needs_path = rules.iter().any(|r| r.query.needs_path());
    let needs_children = rules.iter().any(|r| r.query.needs_children());
    let (mut path, mut chain) = (String::new(), Vec::new());
    let mut found: Vec<(Arc<Rule>, EntryId)> = Vec::new();
    // ponytail: every rule's Query runs on every Entry; index them by name if a 10M-Entry Index makes it slow.
    for (n, id) in (0..index.capacity() as u32)
        .map(EntryId)
        .filter(|&id| index.is_live(id))
        .enumerate()
    {
        if n % 4096 == 0 {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            progress(Phase::Scanning, n as u64, total);
        }
        let is_dir = index.is_dir(id);
        if needs_path {
            index.write_match_path(id, &mut path, &mut chain);
        }
        let candidate = Candidate {
            name: index.name(id),
            path: needs_path.then_some(path.as_str()),
            is_dir,
            logical: index.logical(id),
            modified: index.modified(id),
            depth: 0,
            children: if needs_children && is_dir {
                index.children(id).iter().map(|&c| index.name(c)).collect()
            } else {
                Vec::new()
            },
            ..Candidate::default()
        };
        for rule in rules {
            if (is_dir || rule.files) && rule.query.matches(&candidate) && marker_in_index(index, rule, id) {
                found.push((rule.clone(), id));
            }
        }
    }
    found.retain(|&(_, id)| {
        index
            .volume_of(id)
            .and_then(|v| index.volume(v))
            .is_some_and(|v| volume_blocker(v, volumes).is_none())
    });
    let removed: HashSet<EntryId> = found.iter().filter(|(r, _)| !r.is_command()).map(|&(_, id)| id).collect();
    found.retain(|&(ref rule, id)| {
        let mut up = if rule.is_command() { None } else { index.parent(id) };
        while let Some(a) = up {
            if removed.contains(&a) {
                return false;
            }
            up = index.parent(a);
        }
        true
    });
    let mut seen = HashSet::new();
    let unique: Vec<_> = found
        .into_iter()
        .filter(|(rule, id)| seen.insert((rule.is_command(), rule.name.clone(), *id)))
        .collect();
    let to_size = unique.len() as u64;
    let mut out: Vec<Cleanable> = unique
        .into_iter()
        .enumerate()
        .map(|(n, (rule, id))| {
            progress(Phase::Sizing, n as u64, to_size);
            let sized = rule
                .estimate
                .as_deref()
                .and_then(|e| {
                    Path::new(e)
                        .components()
                        .try_fold(id, |at, c| index.child_named(at, c.as_os_str().to_str()?))
                })
                .unwrap_or(id);
            let project = if rule.markers.iter().any(|m| m.starts_with("..")) {
                index.parent(id).unwrap_or(id)
            } else {
                id
            };
            let (on_disk, exts) = subtree_sizes(index, sized);
            Cleanable {
                path: index.path(id),
                on_disk,
                exts,
                newest: newest_file(index, project),
                rule,
            }
        })
        .collect();
    // The same path found by two path rules is one Cleanable; the earlier rule has it.
    let mut paths = HashSet::new();
    out.retain(|c| c.rule.is_command() || paths.insert(c.path.clone()));
    let group = |c: &Cleanable| Group::ALL.iter().position(|&g| g == c.rule.group);
    out.sort_by(|a, b| {
        group(a)
            .cmp(&group(b))
            .then_with(|| b.on_disk.cmp(&a.on_disk))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// On-disk Size of `id` and everything beneath it, and its files' totals per lower-cased extension.
fn subtree_sizes(index: &Index, id: EntryId) -> (u64, Vec<(String, Totals)>) {
    let (mut total, mut by_ext) = (0, HashMap::<String, Totals>::new());
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        let on_disk = index.on_disk(current);
        total += on_disk;
        if index.is_dir(current) {
            stack.extend_from_slice(index.children(current));
        } else {
            let ext = extension(index.name(current)).unwrap_or("");
            let totals = match by_ext.get_mut(ext) {
                Some(t) => t,
                None => by_ext.entry(ext.to_ascii_lowercase()).or_default(),
            };
            totals.add(Totals {
                logical: index.logical(current),
                on_disk,
                files: 1,
            });
        }
    }
    (total, by_ext.into_iter().collect())
}

/// A symbolic link, or on Windows also a junction or any other reparse point.
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// A plain file: a link could point anywhere.
fn plain(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && !is_link(&m))
}

/// The newest modification time under `path`, in nanoseconds since the Unix epoch; links are not
/// followed. 0 when nothing can be read.
fn newest_on_disk(path: &Path) -> i64 {
    let (mut newest, mut stack) = (0, vec![path.to_path_buf()]);
    while let Some(current) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&current) else { continue };
        if meta.is_dir() && !is_link(&meta) {
            if let Ok(entries) = std::fs::read_dir(&current) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if let Some(t) = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            newest = newest.max(t.as_nanos() as i64);
        }
    }
    newest
}

fn marker_on_disk(path: &Path, marker: &str) -> bool {
    let p = path.join(marker);
    plain(&p)
        && (p.file_name().is_none_or(|n| n != "CACHEDIR.TAG")
            || std::fs::read(&p).is_ok_and(|b| b.starts_with(CACHEDIR_SIGNATURE.as_bytes())))
}

/// Checks `path` on disk against the rule again: an absolute path that is not a link and has no
/// link among its ancestor folders below the Volume it is on (the Volume's own mount path is
/// trusted, whatever links lead to it), with no Volume mounted on or inside it, that the rule's Query
/// still selects, with a marker present. With `min_age_days` (the Cleanable was ticked for being
/// old) nothing in its project may have changed within that many days.
pub fn recheck(rule: &Rule, path: &Path, volumes: &[VolumeInfo], min_age_days: Option<u64>) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("it is not an absolute path".into());
    }
    // A Volume mounted on or inside it would be emptied too: removal does not stop at mount points.
    if volumes.iter().any(|v| v.mount.starts_with(path)) {
        return Err("a Volume is mounted on or inside it".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| "it no longer exists".to_owned())?;
    if is_link(&meta) {
        return Err("it is now a link".into());
    }
    // Removal through a linked ancestor would reach wherever the link points. Only the folders
    // between the Volume's mount path and `path` count; with no Volume found, all of them do.
    let root = volumes
        .iter()
        .map(|v| &v.mount)
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.as_os_str().len());
    if let Some(link) = path
        .ancestors()
        .skip(1)
        .take_while(|a| root.is_none_or(|r| a.starts_with(r) && *a != r.as_path()))
        .find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| is_link(&m)))
    {
        return Err(format!("a folder above it ({}) is a link", link.display()));
    }
    if !meta.is_dir() && (rule.is_command() || !(meta.is_file() && rule.files)) {
        return Err("it is no longer a folder".into());
    }
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("it has no name")?;
    let full = path.to_string_lossy().replace('\\', "/");
    let children: Vec<String> = if meta.is_dir() && rule.query.needs_children() {
        std::fs::read_dir(path)
            .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let candidate = Candidate {
        name,
        path: Some(&full),
        is_dir: meta.is_dir(),
        logical: meta.len(),
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64),
        depth: 0,
        children: children.iter().map(String::as_str).collect(),
        ..Candidate::default()
    };
    if !rule.query.matches(&candidate) {
        return Err(format!("it no longer matches the rule `{}`", rule.name));
    }
    if !rule.markers.is_empty() && !rule.markers.iter().any(|m| marker_on_disk(path, m)) {
        return Err(format!("its marker ({}) is gone", rule.markers.join(" or ")));
    }
    if let Some(days) = min_age_days {
        let project = if rule.markers.iter().any(|m| m.starts_with("..")) {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let newest = newest_on_disk(project);
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        if newest > 0 && now.saturating_sub(newest) <= (days as i64).saturating_mul(NANOS_PER_DAY) {
            return Err(format!("something in it changed within the last {days} days"));
        }
    }
    Ok(())
}

/// The first path at or under `path` (links not followed) that the user does not own: removing
/// it needs admin rights (ADR-0008). `deep` false judges `path` alone, as for a command rule's
/// working folder. Where ownership cannot be told (no uid, an unreadable folder) it counts as not
/// the user's: the admin step shows the command instead of a Clean that might fail half-way.
#[cfg(unix)]
pub fn foreign(path: &Path, deep: bool) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // A file this process creates is owned by its user: no libc call needed to learn the uid.
    static UID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let Some(uid) = *UID.get_or_init(|| {
        let probe = std::env::temp_dir().join(format!("ve-uid-{}", std::process::id()));
        // `create_new` never follows a link planted at the name.
        let uid = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .and_then(|f| f.metadata())
            .map(|m| m.uid())
            .ok();
        let _ = std::fs::remove_file(&probe);
        uid
    }) else {
        return Some(path.to_path_buf());
    };
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&current) else { continue };
        if meta.uid() != uid {
            return Some(current);
        }
        if deep && meta.is_dir() {
            match std::fs::read_dir(&current) {
                Ok(entries) => stack.extend(entries.flatten().map(|e| e.path())),
                Err(_) => return Some(current),
            }
        }
    }
    None
}

/// Windows has no cheap owner test: what lies under the Windows folder is taken as needing admin.
#[cfg(windows)]
pub fn foreign(path: &Path, _deep: bool) -> Option<PathBuf> {
    // Whole folders, compared without case or a `\\?\` prefix; with no Windows folder known, fail closed.
    let plain = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase().trim_start_matches(r"\\?\"));
    let Some(root) = std::env::var_os("SystemRoot") else {
        return Some(path.to_path_buf());
    };
    plain(path).starts_with(plain(Path::new(&root))).then(|| path.to_path_buf())
}

/// Whether the app can run the admin step itself: only through `pkexec`, on Linux.
pub const CAN_ELEVATE: bool = cfg!(target_os = "linux");

/// `rm` by absolute path: pkexec runs it as root, so it must not be looked up via `PATH`.
const RM: &str = "/usr/bin/rm";

/// The exact command that removes `paths` with admin rights, as shown before it runs: with
/// `pkexec` on Linux (what the app runs), otherwise what the user would run themselves.
pub fn admin_command(paths: &[PathBuf]) -> String {
    let quoted = |p: &PathBuf| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    if cfg!(windows) {
        paths
            .iter()
            .map(|p| match p.is_dir() {
                true => format!("rd /s /q \"{}\"", p.display()),
                false => format!("del /f /q \"{}\"", p.display()),
            })
            .collect::<Vec<_>>()
            .join(" & ")
    } else {
        let lead = if cfg!(target_os = "linux") {
            format!("pkexec {RM}")
        } else {
            "sudo rm".to_owned()
        };
        format!("{lead} -rf -- {}", paths.iter().map(quoted).collect::<Vec<_>>().join(" "))
    }
}

/// The admin step: rechecks each Cleanable as Clean does, then removes the ones that still match
/// with one `pkexec rm` (a single password prompt) and reports per Cleanable whether it is gone.
/// Command rules are never run elevated. `on_done` is told how many are rechecked.
pub fn admin_clean(plan: &[(Arc<Rule>, PathBuf, Option<u64>)], volumes: &[VolumeInfo], on_done: &dyn Fn(usize)) -> Outcome {
    let mut out = Outcome::default();
    let mut go: Vec<PathBuf> = Vec::new();
    for (k, (rule, path, min_age_days)) in plan.iter().enumerate() {
        if rule.is_command() {
            out.skipped
                .push((path.clone(), "command rules are not run with admin rights".into()));
        } else if let Err(why) = recheck(rule, path, volumes, *min_age_days) {
            out.skipped.push((path.clone(), why));
        } else {
            go.push(path.clone());
        }
        on_done(k + 1);
    }
    if go.is_empty() {
        return out;
    }
    let why = if !CAN_ELEVATE {
        "this system cannot run it elevated from here; run the command shown".to_owned()
    } else {
        match Command::new("pkexec").arg(RM).args(["-rf", "--"]).args(&go).status() {
            Err(e) => format!("could not start pkexec ({e})"),
            // pkexec: 126 = the password prompt was dismissed, 127 = not authorized.
            Ok(s) => match s.code() {
                Some(0) => String::new(),
                Some(126) => "cancelled".to_owned(),
                Some(127) => "not authorized".to_owned(),
                _ => format!("rm failed ({s})"),
            },
        }
    };
    for path in go {
        if std::fs::symlink_metadata(&path).is_err() {
            out.removed.push(path);
        } else {
            out.skipped
                .push((path, if why.is_empty() { "it is still there".into() } else { why.clone() }));
        }
    }
    out
}

/// What running one command did.
#[derive(Debug)]
pub struct CommandReport {
    pub argv: String,
    pub cwd: PathBuf,
    /// Exit status and the end of its output, or why it did not run.
    pub result: Result<(Option<i32>, String), String>,
}

impl CommandReport {
    pub fn succeeded(&self) -> bool {
        matches!(self.result, Ok((Some(0), _)))
    }
}

/// What Clean did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: Vec<PathBuf>,
    /// Paths left alone, each with why.
    pub skipped: Vec<(PathBuf, String)>,
    pub commands: Vec<CommandReport>,
}

/// Runs `argv` without a shell in `cwd`, capturing its output.
fn run(argv: &[String], cwd: &Path) -> Result<(Option<i32>, String), String> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("`{}` was not found; is it installed and on the PATH?", argv[0]),
            _ => format!("`{}` could not be started: {e}", argv[0]),
        })?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim();
    let start = text.len().saturating_sub(OUTPUT_LIMIT);
    let start = (start..=text.len()).find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    Ok((out.status.code(), text[start..].to_owned()))
}

/// Rechecks each Cleanable and applies the rule to the ones that still match: a command runs, a
/// path is removed permanently. `remove_dir_all` never follows links, so a folder swapped for a
/// link after the recheck loses only the link. `on_done` is told how many are finished.
pub fn clean(plan: &[(Arc<Rule>, PathBuf, Option<u64>)], volumes: &[VolumeInfo], on_done: &dyn Fn(usize)) -> Outcome {
    let mut out = Outcome::default();
    for (k, (rule, path, min_age_days)) in plan.iter().enumerate() {
        if let Err(why) = recheck(rule, path, volumes, *min_age_days) {
            out.skipped.push((path.clone(), why));
        } else if let Some(other) = foreign(path, !rule.is_command()) {
            out.skipped
                .push((path.clone(), format!("{} is not yours; it needs the admin step", other.display())));
        } else if rule.is_command() {
            out.commands.push(CommandReport {
                argv: rule.argv_text(),
                cwd: path.clone(),
                result: run(&rule.command, path),
            });
        } else {
            let gone = if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            match gone {
                Ok(()) => out.removed.push(path.clone()),
                Err(e) => out.skipped.push((path.clone(), e.to_string())),
            }
        }
        on_done(k + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::{EntryMeta, SyncState};

    const OS: &str = "linux";

    fn context() -> QueryContext {
        QueryContext::new(CategoryMap::builtin(), chrono::Local::now())
    }

    fn builtin(os: &str) -> Rules {
        parse(BUILTIN, "built-in rules", os, &context())
    }

    fn rule(name: &str) -> Arc<Rule> {
        builtin(OS).rules.iter().find(|r| r.name == name).unwrap().clone()
    }

    /// The temp folder as a Volume mount path would read: real, with no links (macOS keeps it under
    /// the /var link) and no Windows `\\?\` prefix.
    fn real_temp() -> PathBuf {
        let real = std::env::temp_dir().canonicalize().unwrap();
        PathBuf::from(real.to_string_lossy().trim_start_matches(r"\\?\"))
    }

    fn volume_at(mount: &Path) -> VolumeInfo {
        VolumeInfo {
            mount: mount.to_path_buf(),
            identity: "uuid:m".into(),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: true,
            network: false,
            read_only: false,
        }
    }

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    fn file(size: u64) -> EntryMeta {
        EntryMeta {
            on_disk: size,
            ..Default::default()
        }
    }

    #[test]
    fn subtree_sizes_split_files_by_lowercased_extension() {
        let mut index = Index::new();
        let vol = index.add_volume(Path::new("/"), "ext4", "uuid:root", dir(), SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        let sub = index.insert(root, "target", dir());
        index.insert(sub, "a.RS", file(10));
        index.insert(sub, "b.rs", file(5));
        index.insert(sub, "Makefile", file(1));
        let (total, exts) = subtree_sizes(&index, sub);
        let get = |e: &str| exts.iter().find(|(x, _)| x == e).map(|(_, t)| (t.on_disk, t.files));
        assert_eq!(total, 16);
        assert_eq!((get("rs"), get("")), (Some((15, 2)), Some((1, 1))));
    }

    #[test]
    fn builtin_rules_parse_on_every_platform_with_a_marker_or_location_test() {
        for os in ["linux", "macos", "windows"] {
            let rules = builtin(os);
            assert!(rules.errors.is_empty(), "{os}: {:?}", rules.errors);
            assert!(!rules.rules.is_empty());
            for g in Group::ALL {
                assert!(rules.rules.iter().any(|r| r.group == g), "{os} has no {g:?} rule");
            }
        }
        let raw: File = toml::from_str(BUILTIN).unwrap();
        for r in raw.rule {
            assert!(
                !r.markers.is_empty() || r.query.contains("parent:"),
                "{} has no marker or location test",
                r.name
            );
        }
        // Rules are filtered by platform.
        let names = |os| builtin(os).rules.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert!(names("macos").contains(&"Xcode DerivedData".to_owned()));
        assert!(!names("linux").contains(&"Xcode DerivedData".to_owned()));
        assert!(!names("windows").contains(&"User cache folder".to_owned()));
        assert!(names("windows").contains(&"Windows cache folder".to_owned()));
        assert!(!names("linux").contains(&"Windows cache folder".to_owned()));
        assert!(names("windows").contains(&"Crash dumps".to_owned()));
    }

    #[test]
    fn user_file_errors_name_the_file_line_and_reason() {
        let ctx = context();
        let bad_syntax = parse("[[rule]]\nname = \"x\"\ngroup = = \"temp\"\n", "cleanup.toml", OS, &ctx);
        assert!(bad_syntax.rules.is_empty());
        assert!(bad_syntax.errors[0].starts_with("cleanup.toml line 3: "), "{:?}", bad_syntax.errors);
        let text = "[[rule]]\nname = \"ok\"\ngroup = \"temp\"\nmarkers = [\"m\"]\nquery = \"foo\"\n\n[[rule]]\nname = \"bad\"\ngroup = \"temp\"\nquery = \"size:abc\"\n";
        let two = parse(text, "cleanup.toml", OS, &ctx);
        assert_eq!(two.rules.len(), 1, "{:?}", two.errors);
        assert!(
            two.errors
                .iter()
                .any(|e| e.starts_with("cleanup.toml line 7: rule 2: `bad`: query `size:abc`")),
            "{:?}",
            two.errors
        );
        let group = parse(
            "[[rule]]\nname = \"w\"\ngroup = \"nope\"\nquery = \"x\"\n",
            "cleanup.toml",
            OS,
            &ctx,
        );
        assert!(group.errors[0].starts_with("cleanup.toml line 3: "), "{:?}", group.errors);
        let unknown = parse(
            "[[rule]]\nname=\"a\"\ngroup=\"temp\"\nquery=\"x\"\ncolour=1\n",
            "cleanup.toml",
            OS,
            &ctx,
        );
        assert!(unknown.errors[0].contains("colour"), "{:?}", unknown.errors);
        // A user file that is missing or broken leaves the built-in rules.
        let dir = std::env::temp_dir().join(format!("ve-rules-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let broken = dir.join("cleanup.toml");
        std::fs::write(&broken, "[[rule\n").unwrap();
        let loaded = load(Some(&broken));
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.rules.len(), load(None).rules.len());
        std::fs::write(&broken, "[[rule]]\nname=\"mine\"\ngroup=\"cache\"\nmarkers=[\"m\"]\nquery=\"x\"\n").unwrap();
        assert_eq!(load(Some(&broken)).rules.len(), load(None).rules.len() + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_marked_cleanables_on_writable_locations() {
        let mut index = Index::new();
        let vol = index.add_volume(Path::new("/w"), "ext4", "uuid:w", dir(), SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        let at = |index: &mut Index, parent, name, meta| index.insert(parent, name, meta);
        // Sibling Cargo.toml; a nested target inside it is not listed separately.
        let a = at(&mut index, root, "a", dir());
        at(&mut index, a, "Cargo.toml", file(10));
        let target = at(&mut index, a, "target", dir());
        at(&mut index, target, "big", file(4096));
        let inner = at(&mut index, target, "target", dir());
        at(&mut index, inner, "CACHEDIR.TAG", file(1));
        // Marker inside, no Cargo.toml.
        let b = at(&mut index, root, "b", dir());
        let tagged = at(&mut index, b, "target", dir());
        at(&mut index, tagged, "CACHEDIR.TAG", file(8));
        // Unmarked `target`, and a Cargo.toml *folder* doesn't count.
        let c = at(&mut index, root, "c", dir());
        at(&mut index, c, "target", dir());
        let d = at(&mut index, root, "d", dir());
        at(&mut index, d, "Cargo.toml", dir());
        at(&mut index, d, "target", dir());
        // A Next.js project: .next goes with its package.json, and node_modules is offered unticked.
        let n = at(&mut index, root, "n", dir());
        at(&mut index, n, "package.json", file(1));
        let next = at(&mut index, n, ".next", dir());
        at(&mut index, next, "x", file(100));
        let nm = at(&mut index, n, "node_modules", dir());
        at(&mut index, nm, "y", file(200));
        let m = at(&mut index, root, "m", dir());
        at(&mut index, m, ".next", dir());

        let rules = builtin(OS).rules;
        let found = find(&index, &[], &rules, &AtomicBool::new(false), &|_, _, _| {});
        let listed: Vec<_> = found.iter().map(|c| (c.rule.name.as_str(), c.path.clone())).collect();
        assert_eq!(
            listed,
            vec![
                ("Rust build artefacts", PathBuf::from("/w/a/target")),
                ("node_modules", PathBuf::from("/w/n/node_modules")),
                ("Next.js build output", PathBuf::from("/w/n/.next")),
                ("Rust build artefacts", PathBuf::from("/w/b/target")),
            ]
        );
        assert_eq!(found[0].on_disk, 4096 + 1);
        assert_eq!(
            found.iter().find(|c| c.rule.name == "node_modules").map(|c| c.rule.tick),
            Some(false)
        );

        // Read-only mounts, Offline Volumes and Remotes are never offered.
        let ro = VolumeInfo {
            mount: "/w".into(),
            identity: "uuid:w".into(),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: false,
            network: false,
            read_only: true,
        };
        let no_cancel = AtomicBool::new(false);
        assert!(find(&index, &[ro], &rules, &no_cancel, &|_, _, _| {}).is_empty());
        index.set_mount(vol, None);
        assert!(find(&index, &[], &rules, &no_cancel, &|_, _, _| {}).is_empty());
        let remote = index.add_remote(&ve_core::Remote::new("me@box", "/srv").unwrap());
        let rroot = index.volume(remote).unwrap().root;
        let r = at(&mut index, rroot, "p", dir());
        at(&mut index, r, "Cargo.toml", file(1));
        at(&mut index, r, "target", dir());
        assert!(find(&index, &[], &rules, &AtomicBool::new(false), &|_, _, _| {}).is_empty());
    }

    #[test]
    fn location_rules_list_what_is_in_the_folder_and_ages_decide_ticking() {
        let tmp = std::env::temp_dir();
        let top = tmp.ancestors().last().unwrap();
        let mut index = Index::new();
        let vol = index.add_volume(top, "ext4", "uuid:root", dir(), SyncState::Walk);
        let mut parent = index.volume(vol).unwrap().root;
        for part in tmp.strip_prefix(top).unwrap().components() {
            parent = index.insert(parent, part.as_os_str().to_str().unwrap(), dir());
        }
        let day = 86_400 * 1_000_000_000i64;
        let now = 100 * day;
        let old = index.insert(
            parent,
            "old.log",
            EntryMeta {
                modified: now - 8 * day,
                ..file(5)
            },
        );
        let fresh = index.insert(
            parent,
            "fresh.log",
            EntryMeta {
                modified: now - day,
                ..file(5)
            },
        );
        index.insert(parent, ".X11-unix", dir());
        let found = find(&index, &[], &builtin(OS).rules, &AtomicBool::new(false), &|_, _, _| {});
        let names: Vec<_> = found.iter().map(|c| c.path.file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let get = |id| found.iter().find(|c| c.path == index.path(id)).unwrap();
        assert!(get(old).pre_ticked(now, 7, 30));
        assert!(!get(fresh).pre_ticked(now, 7, 30));
        assert!(!get(fresh).pre_ticked(now, 1, 30) && get(fresh).pre_ticked(now + 2 * day, 1, 30));
        // Build artefacts follow the other limit; node_modules and caches never tick.
        let c = |name: &str, newest| Cleanable {
            rule: rule(name),
            path: "/x".into(),
            on_disk: 0,
            exts: Vec::new(),
            newest,
        };
        assert!(c("Rust build artefacts", now - 31 * day).pre_ticked(now, 7, 30));
        assert!(!c("Rust build artefacts", now - 29 * day).pre_ticked(now, 7, 30));
        assert!(!c("Rust build artefacts", 0).pre_ticked(now, 7, 30));
        assert!(!c("node_modules", 0).pre_ticked(now, 7, 30));
        assert!(!c("User cache folder", 0).pre_ticked(now, 7, 30));
    }

    #[test]
    fn the_readme_example_is_a_valid_user_file() {
        // A Windows checkout may have CRLF line ends.
        let readme = include_str!("../../../../README.md").replace("\r\n", "\n");
        let start = readme.find("```toml\n[[rule]]").unwrap() + "```toml\n".len();
        let example = &readme[start..start + readme[start..].find("```").unwrap()];
        let rules = parse(example, "cleanup.toml", OS, &context());
        assert!(rules.errors.is_empty(), "{:?}", rules.errors);
        assert_eq!(rules.rules.len(), 2);
        assert!(rules.rules[1].is_command());
    }

    #[test]
    fn quoted_path_with_spaces_parses() {
        assert!(Query::parse("parent:\"/a b\" !regex:^\\.", &context()).is_ok());
    }

    #[test]
    fn clean_rechecks_on_disk() {
        let base = real_temp().join(format!("ve-cleaner-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let make = |name: &str, cargo: bool, tag: Option<&str>| {
            let target = base.join(name).join("target");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("artefact"), "x").unwrap();
            if cargo {
                std::fs::write(base.join(name).join("Cargo.toml"), "").unwrap();
            }
            if let Some(tag) = tag {
                std::fs::write(target.join("CACHEDIR.TAG"), tag).unwrap();
            }
            target
        };
        let rust = rule("Rust build artefacts");
        let vols = [volume_at(&real_temp())];
        let rc = |p: &Path| recheck(&rust, p, &vols, None);
        let cargo = make("cargo", true, None);
        let tagged = make("tagged", false, Some(CACHEDIR_SIGNATURE));
        let fake_tag = make("faketag", false, Some("nope"));
        let unmarked = make("unmarked", false, None);
        let vanished = base.join("vanished").join("target");

        let plan: Vec<_> = [&cargo, &tagged, &fake_tag, &unmarked, &vanished]
            .iter()
            .map(|p| (rust.clone(), (*p).clone(), None))
            .collect();
        let out = clean(&plan, &vols, &|_| {});
        assert_eq!(out.removed, vec![cargo.clone(), tagged.clone()]);
        assert!(!cargo.exists() && !tagged.exists());
        assert!(fake_tag.exists() && unmarked.exists());
        assert_eq!(out.skipped.len(), 3);
        assert!(out.skipped.iter().any(|(p, _)| *p == vanished));
        // A marker that vanished since the Index was built skips the folder too.
        std::fs::remove_file(fake_tag.join("CACHEDIR.TAG")).unwrap();
        assert!(rc(&fake_tag).is_err());
        // A folder with another name is not what the rule found.
        let renamed = base.join("cargo").join("other");
        std::fs::create_dir_all(&renamed).unwrap();
        std::fs::write(base.join("cargo").join("Cargo.toml"), "").unwrap();
        assert!(rc(&renamed).is_err());
        // A symlinked Cargo.toml is no marker, and a symlinked target is never removed.
        #[cfg(unix)]
        {
            let linked = make("linked", false, None);
            std::os::unix::fs::symlink(base.join("unmarked").join("target"), base.join("linked").join("Cargo.toml")).unwrap();
            assert!(rc(&linked).is_err());
            let alias = base.join("alias").join("target");
            std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
            std::fs::write(alias.parent().unwrap().join("Cargo.toml"), "").unwrap();
            std::os::unix::fs::symlink(&unmarked, &alias).unwrap();
            assert!(rc(&alias).is_err());
        }
        // A Volume mounted inside, a relative path and a symlinked CACHEDIR.TAG are all refused.
        let mounted = make("mounted", true, None);
        assert!(recheck(&rust, &mounted, &[volume_at(&mounted.join("deep"))], None).is_err());
        assert!(rc(Path::new("target")).is_err());
        #[cfg(unix)]
        {
            let tagged_link = make("taglink", false, None);
            let real = base.join("real.tag");
            std::fs::write(&real, CACHEDIR_SIGNATURE).unwrap();
            std::os::unix::fs::symlink(&real, tagged_link.join("CACHEDIR.TAG")).unwrap();
            assert!(rc(&tagged_link).is_err());
        }
        // A link among the ancestors is refused, and so is a project changed within the age limit.
        #[cfg(unix)]
        {
            let real = make("real-parent", true, None);
            let via = base.join("via");
            std::os::unix::fs::symlink(real.parent().unwrap(), &via).unwrap();
            let err = rc(&via.join("target")).unwrap_err();
            assert!(err.contains("is a link"), "{err}");
            // A link above the Volume's mount path is trusted: only what is below it counts.
            assert!(recheck(&rust, &via.join("target"), &[volume_at(&via)], None).is_ok());
            // With no Volume known, every ancestor counts.
            assert!(recheck(&rust, &via.join("target"), &[], None).is_err());
        }
        let fresh = make("fresh", true, None);
        assert!(recheck(&rust, &fresh, &vols, Some(30)).unwrap_err().contains("changed within"));
        assert!(recheck(&rust, &fresh, &vols, Some(0)).is_ok());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn command_rules_run_without_a_shell_and_report_failures() {
        let base = real_temp().join(format!("ve-cleaner-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("marker"), "").unwrap();
        let vols = [volume_at(&real_temp())];
        let ctx = context();
        let mk = |command: &str| {
            let text = format!(
                "[[rule]]\nname = \"c\"\ngroup = \"build\"\nquery = 'regex:^proj$'\nmarkers = [\"marker\"]\ncommand = {command}\nestimate = \"sub/dir\"\n"
            );
            let r = parse(&text, "cleanup.toml", OS, &ctx);
            assert!(r.errors.is_empty(), "{:?}", r.errors);
            r.rules[0].clone()
        };
        let ok = mk(r#"["sh", "-c", "echo $0 hi; rm marker2", ";$(touch pwned)"]"#);
        let bad = mk(r#"["sh", "-c", "echo oops >&2; exit 3"]"#);
        let missing = mk(r#"["definitely-not-a-tool", "--x"]"#);
        let out = clean(
            &[
                (ok.clone(), project.clone(), None),
                (bad, project.clone(), None),
                (missing, project.clone(), None),
            ],
            &vols,
            &|_| {},
        );
        assert_eq!(out.commands.len(), 3);
        assert!(!out.commands[0].succeeded());
        // `;$(touch pwned)` was a plain argument, not shell code.
        assert!(!project.join("pwned").exists());
        assert!(out.commands[0].result.as_ref().unwrap().1.contains(";$(touch pwned) hi"));
        let (code, text) = out.commands[1].result.as_ref().unwrap();
        assert_eq!((*code, text.as_str()), (Some(3), "oops"));
        assert!(out.commands[2].result.as_ref().unwrap_err().contains("not found"));
        // The argv is shown exactly, quoting what has spaces.
        assert_eq!(mk(r#"["a", "b c"]"#).argv_text(), "a \"b c\"");
        // A command rule whose marker is gone does not run.
        std::fs::remove_file(project.join("marker")).unwrap();
        let out = clean(&[(ok, project, None)], &vols, &|_| {});
        assert!(out.commands.is_empty() && out.skipped.len() == 1);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn what_the_user_does_not_own_goes_to_the_admin_step() {
        let base = std::env::temp_dir().join(format!("ve-cleaner-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("target")).unwrap();
        assert_eq!(foreign(&base, true), None);
        // Not run as root: /usr is root's, and so is anything inside a root-owned folder.
        if std::fs::metadata("/usr").is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0) && foreign(&base, false).is_none() {
            assert_eq!(foreign(Path::new("/usr"), false), Some(PathBuf::from("/usr")));
            assert!(foreign(Path::new("/usr"), true).is_some());
        }
        // The admin command names every path, quoted; a vanished one is reported and nothing is started.
        let paths = [base.join("a b"), base.join("c")];
        let text = admin_command(&paths);
        assert!(
            text.contains(&format!("'{}'", paths[0].display())) && text.contains("rm -rf --"),
            "{text}"
        );
        let rust = rule("Rust build artefacts");
        let out = admin_clean(&[(rust, base.join("target"), None)], &[], &|_| {});
        assert!(out.removed.is_empty() && out.skipped.len() == 1, "{out:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
