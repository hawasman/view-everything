//! `ve`: reads the saved Index (never writes it) and prints Entries, largest folders or CSV.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use ve_core::{CategoryMap, EntryId, Filter, Index, Query, QueryContext, export::write_csv};

const DEFAULT_LIMIT: usize = 20;

const USAGE: &str = "\
Usage:
  ve find [OPTIONS] [--] <QUERY>...   Entries matching a Query, one path per line
  ve top  [OPTIONS] [--] [QUERY]...   Largest folders (or --files) by On-disk Size (Filtered Size under a Query)
  ve du   ...                         Same as top
  ve export [OPTIONS] [--] [QUERY]... CSV of a Query's results (everything without one; --json for JSON)

Options:
  --csv           CSV output (path, name, Logical Size, On-disk Size, modified, Category)
  --json          JSON array output
  --path <PATH>   Only Entries under PATH
  --index <FILE>  Read this saved Index instead of the app's
  -n <N>          top/du: how many rows (default 20, 0 for all)
  --files         top/du: largest files instead of folders

Exit codes: 0 success, 1 no match, 2 error. Last Synced is reported on stderr.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Find,
    Top,
    Export,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Plain,
    Csv,
    Json,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    command: Command,
    format: Format,
    query: String,
    path: Option<PathBuf>,
    index: Option<PathBuf>,
    limit: Option<usize>,
    files: bool,
}

/// `Ok(None)` means help was asked for.
fn parse_args(args: &[String]) -> Result<Option<Args>, String> {
    let mut it = args.iter();
    let command = match it.next().map(String::as_str) {
        Some("find") => Command::Find,
        Some("top" | "du") => Command::Top,
        Some("export") => Command::Export,
        Some("-h" | "--help" | "help") => return Ok(None),
        Some(other) => return Err(format!("unknown command `{other}`")),
        None => return Err("missing command".into()),
    };
    let mut parsed = Args {
        command,
        format: Format::Plain,
        query: String::new(),
        path: None,
        index: None,
        limit: None,
        files: false,
    };
    let mut words = Vec::new();
    let mut format = None;
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--csv" | "--json" => {
                let f = if arg == "--csv" { Format::Csv } else { Format::Json };
                if format.replace(f).is_some_and(|old| old != f) {
                    return Err("--csv and --json cannot be combined".into());
                }
            }
            "--files" => parsed.files = true,
            "--path" => parsed.path = Some(value("--path")?.into()),
            "--index" => parsed.index = Some(value("--index")?.into()),
            "-n" => parsed.limit = Some(value("-n")?.parse().map_err(|_| "-n needs a number".to_string())?),
            "--" => {
                words.extend(it.by_ref().cloned());
            }
            flag if flag.starts_with('-') && flag.len() > 1 => return Err(format!("unknown option `{flag}`")),
            word => words.push(word.to_string()),
        }
    }
    if (parsed.files || parsed.limit.is_some()) && parsed.command != Command::Top {
        return Err("--files and -n only apply to top/du".into());
    }
    parsed.format = format.unwrap_or(if parsed.command == Command::Export {
        Format::Csv
    } else {
        Format::Plain
    });
    parsed.query = words.join(" ");
    if parsed.command == Command::Find && parsed.query.trim().is_empty() {
        return Err("find needs a Query".into());
    }
    Ok(Some(parsed))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_args(&args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ve: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let stdout = io::stdout().lock();
    match run(&parsed, &mut io::BufWriter::new(stdout), &mut io::stderr()) {
        Ok(0) => ExitCode::from(1),
        Ok(_) => ExitCode::SUCCESS,
        // A closed pipe (`ve find x | head`) is not a failure.
        Err(Failure::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ve: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum Failure {
    Io(io::Error),
    Message(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Io(e) => e.fmt(f),
            Failure::Message(m) => f.write_str(m),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Io(e)
    }
}

/// Runs a command, returning how many Entries it printed.
fn run(args: &Args, out: &mut impl Write, err: &mut impl Write) -> Result<usize, Failure> {
    let path = match &args.index {
        Some(path) => path.clone(),
        None => ve_core::paths::index_path().ok_or_else(|| Failure::Message("no data folder on this system".into()))?,
    };
    let index = Index::load(&path).map_err(|e| {
        Failure::Message(format!(
            "cannot read the Index at {}: {e} (open the app once to build it)",
            path.display()
        ))
    })?;
    writeln!(err, "Last Synced: {}", last_synced(&index))?;
    let categories = ve_core::paths::load_categories().map_err(Failure::Message)?;
    let context = QueryContext::new(categories.clone(), chrono::Local::now());
    let query = match args.query.trim() {
        "" => None,
        text => Some(Query::parse(text, &context).map_err(|e| Failure::Message(format!("bad Query: {e}")))?),
    };
    if query.as_ref().is_some_and(Query::needs_duplicates) {
        writeln!(err, "ve: dupe: finds nothing here; the CLI does not read the app's hashes")?;
    }
    let filter = Filter::compute(&index, query.as_ref());
    let scope = scope(&index, args.path.as_deref())?;
    let mut ids: Vec<EntryId> = match args.command {
        Command::Find | Command::Export => scope.into_iter().filter(|&id| query.is_none() || filter.is_matched(id)).collect(),
        Command::Top if args.files => scope.into_iter().filter(|&id| !index.is_dir(id) && filter.is_counted(id)).collect(),
        Command::Top => scope
            .into_iter()
            .filter(|&id| index.is_dir(id) && filter.totals(&index, id).files > 0)
            .collect(),
    };
    if args.command == Command::Top {
        ids.sort_by_cached_key(|&id| (std::cmp::Reverse(filter.totals(&index, id).on_disk), index.path(id)));
        match args.limit.unwrap_or(DEFAULT_LIMIT) {
            0 => {}
            n => ids.truncate(n),
        }
    } else {
        ids.sort_by_cached_key(|&id| index.path(id));
    }
    write_entries(out, args, &index, &filter, &categories, &ids)?;
    out.flush()?;
    Ok(ids.len())
}

fn write_entries(
    out: &mut impl Write,
    args: &Args,
    index: &Index,
    filter: &Filter,
    categories: &CategoryMap,
    ids: &[EntryId],
) -> Result<(), Failure> {
    match args.format {
        Format::Csv => write_csv(out, index, filter, categories, ids)?,
        Format::Json => {
            let rows: Vec<_> = ids
                .iter()
                .map(|&id| {
                    let totals = filter.totals(index, id);
                    let name = index.name(id);
                    serde_json::json!({
                        "path": index.path(id).to_string_lossy(),
                        "name": name,
                        "is_dir": index.is_dir(id),
                        "logical_size": totals.logical,
                        "on_disk_size": totals.on_disk,
                        "modified_ns": index.modified(id),
                        "category": (!index.is_dir(id)).then(|| categories.of_name(name).name()),
                    })
                })
                .collect();
            serde_json::to_writer_pretty(&mut *out, &rows).map_err(io::Error::from)?;
            writeln!(out)?;
        }
        Format::Plain => {
            for &id in ids {
                if args.command == Command::Top {
                    write!(out, "{}\t", filter.totals(index, id).on_disk)?;
                }
                writeln!(out, "{}", index.path(id).display())?;
            }
        }
    }
    Ok(())
}

/// Live Entries under `path` (not `path` itself), or every live Entry without one.
fn scope(index: &Index, path: Option<&std::path::Path>) -> Result<Vec<EntryId>, Failure> {
    let Some(path) = path else {
        return Ok((0..index.capacity() as u32).map(EntryId).filter(|&id| index.is_live(id)).collect());
    };
    let root = index
        .find_path(path)
        .ok_or_else(|| Failure::Message(format!("{} is not in the Index", path.display())))?;
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for &child in index.children(dir) {
            if index.is_live(child) {
                found.push(child);
                if index.is_dir(child) {
                    stack.push(child);
                }
            }
        }
    }
    Ok(found)
}

/// The app's overall Last Synced: its least recently synced Location's.
fn last_synced(index: &Index) -> String {
    match index.oldest_synced() {
        None => "never (the Index has no Locations)".into(),
        Some(None) => "never (a Location has not finished its first sync)".into(),
        Some(Some(at)) => match chrono::DateTime::from_timestamp(at, 0) {
            Some(t) => t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %:z").to_string(),
            None => "unknown".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::{EntryMeta, SyncState};

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn parse(line: &str) -> Args {
        parse_args(&args(line)).unwrap().unwrap()
    }

    #[test]
    fn parses_commands_flags_and_query_words() {
        let a = parse("find --csv --path /w *.txt size:>1k");
        assert_eq!((a.command, a.format, a.path), (Command::Find, Format::Csv, Some("/w".into())));
        assert_eq!(a.query, "*.txt size:>1k");
        let a = parse("du -n 5 --files --json");
        assert_eq!((a.command, a.limit, a.files, a.format), (Command::Top, Some(5), true, Format::Json));
        assert_eq!(parse("find -- -x").query, "-x");
        assert_eq!((parse("export").query.as_str(), parse("export").format), ("", Format::Csv));
    }

    #[test]
    fn rejects_bad_arguments() {
        for line in [
            "",
            "nope",
            "find",
            "find --csv --json x",
            "find --bogus x",
            "top -n x",
            "top -n",
            "find --files x",
            "export -n 3",
        ] {
            assert!(parse_args(&args(line)).is_err(), "{line}");
        }
        assert!(parse_args(&args("find --help")).unwrap().is_none());
    }

    fn fixture(dir: &std::path::Path) -> PathBuf {
        let mut index = Index::new();
        let folder = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let vol = index.add_volume(std::path::Path::new("/w"), "ext4", "uuid:w", folder, SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        let sub = index.insert(root, "sub", folder);
        let file = |size| EntryMeta {
            logical: size,
            on_disk: size,
            modified: 1_791_646_413_000_000_000,
            ..Default::default()
        };
        index.insert(root, "a.txt", file(10));
        index.insert(sub, "b.txt", file(500));
        index.insert(sub, "=evil.log", file(20));
        index.mark_synced(vol, 1_791_646_413, None);
        let path = dir.join("index.bin");
        index.save(&path).unwrap();
        path
    }

    /// The fixture's path for `/w/<rel>`, as the Index prints it: the root verbatim, then the platform's separator.
    fn w(rel: &str) -> String {
        let mut path = PathBuf::from("/w");
        path.extend(rel.split('/').filter(|c| !c.is_empty()));
        path.display().to_string()
    }

    fn run_line(line: &str, index: &std::path::Path) -> (Result<usize, Failure>, String, String) {
        let mut a = parse(line);
        a.index = Some(index.into());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let n = run(&a, &mut out, &mut err);
        (n, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn find_prints_paths_and_reports_last_synced() {
        let dir = tempfile::tempdir().unwrap();
        let index = fixture(dir.path());
        let (n, out, err) = run_line("find *.txt", &index);
        assert_eq!(n.unwrap(), 2);
        assert_eq!(out, format!("{}\n{}\n", w("a.txt"), w("sub/b.txt")));
        assert!(err.starts_with("Last Synced: 20"), "{err}");
        assert_eq!(run_line("find *.nothing", &index).0.unwrap(), 0);
        assert!(matches!(run_line("find size:", &index).0, Err(Failure::Message(_))));
    }

    #[test]
    fn find_writes_csv_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let index = fixture(dir.path());
        let (_, csv, _) = run_line("find --csv =evil*", &index);
        assert!(
            csv.starts_with("path,name,Logical Size,On-disk Size,modified,Category\r\n"),
            "{csv}"
        );
        assert!(
            csv.contains(&format!("{},'=evil.log,20,20,2026-10-10T15:33:33Z,", w("sub/=evil.log"))),
            "{csv}"
        );
        let (_, json, _) = run_line("find --json b.txt", &index);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["path"], w("sub/b.txt"));
        assert_eq!(v[0]["on_disk_size"], 500);
    }

    #[test]
    fn top_ranks_folders_and_files_by_on_disk_size() {
        let dir = tempfile::tempdir().unwrap();
        let index = fixture(dir.path());
        let (_, out, _) = run_line("top", &index);
        assert_eq!(out, format!("530\t{}\n520\t{}\n", w(""), w("sub")));
        let (_, out, _) = run_line("du --files -n 1", &index);
        assert_eq!(out, format!("500\t{}\n", w("sub/b.txt")));
        let (_, out, _) = run_line("top --path /w a.txt", &index);
        assert_eq!(out, "");
        let (n, out, _) = run_line("top --path /w --files .log", &index);
        assert_eq!((n.unwrap(), out), (1, format!("20\t{}\n", w("sub/=evil.log"))));
        assert!(matches!(run_line("top --path /nowhere", &index).0, Err(Failure::Message(_))));
    }

    #[test]
    fn find_path_scopes_and_top_takes_the_output_flags() {
        let dir = tempfile::tempdir().unwrap();
        let index = fixture(dir.path());
        let (_, out, _) = run_line(&format!("find --path {} txt", w("sub")), &index);
        assert_eq!(out, format!("{}\n", w("sub/b.txt")));
        let (_, csv, _) = run_line("top --csv --files -n 1", &index);
        assert!(csv.contains(&format!("{},b.txt,500,500,", w("sub/b.txt"))), "{csv}");
        let (_, json, _) = run_line("du --json -n 1", &index);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap()[0]["path"], w(""));
        let (_, _, err) = run_line("find dupe:", &index);
        assert!(err.contains("dupe:"), "{err}");
    }

    #[test]
    fn export_without_a_query_lists_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let index = fixture(dir.path());
        let (n, csv, _) = run_line("export", &index);
        assert_eq!(n.unwrap(), 5);
        assert_eq!(csv.lines().count(), 6);
    }

    #[test]
    fn a_missing_index_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_line("find x", &dir.path().join("none.bin")).0,
            Err(Failure::Message(_))
        ));
    }
}
