//! CSV export of a list of Entries, shared by the app and the CLI.

use crate::{CategoryMap, EntryId, Filter, Index};
use std::io::{self, Write};

/// Writes one CSV row per live Entry of `ids`: path, name, Logical Size, On-disk Size, modified
/// time (UTC, ISO 8601; empty when unknown), Category (empty for folders), Owner and Owner kind
/// (both empty when the Entry has no Owner). Sizes are Filtered Sizes under `filter`, as the
/// Explorer shows them.
pub fn write_csv(out: &mut impl Write, index: &Index, filter: &Filter, categories: &CategoryMap, ids: &[EntryId]) -> io::Result<()> {
    out.write_all(HEADER)?;
    for &id in ids.iter().filter(|&&id| index.is_live(id)) {
        let totals = filter.totals(index, id);
        let name = index.name(id);
        let owner = index.entry_owner(id);
        write_entry(
            out,
            &CsvEntry {
                path: &index.path(id).to_string_lossy(),
                name,
                logical: totals.logical,
                on_disk: totals.on_disk,
                // The Index keeps nanoseconds since the Unix epoch (to the whole second); 0 is unknown.
                modified_ns: Some(index.modified(id)).filter(|&ns| ns != 0),
                category: if index.is_dir(id) { "" } else { categories.of_name(name).name() },
                owner: owner.map_or("", |o| o.name.as_str()),
                owner_kind: owner.map_or("", |o| o.kind.name()),
            },
        )?;
    }
    Ok(())
}

/// The header row [`write_csv`] starts with.
pub const HEADER: &[u8] = b"path,name,Logical Size,On-disk Size,modified,Category,Owner,Owner kind\r\n";

/// One Entry's fields, for a client that has them without the Index (the CLI asking the app).
pub struct CsvEntry<'a> {
    pub path: &'a str,
    pub name: &'a str,
    pub logical: u64,
    pub on_disk: u64,
    /// Nanoseconds since the Unix epoch; `None` when unknown.
    pub modified_ns: Option<i64>,
    /// Empty for folders.
    pub category: &'a str,
    /// Empty without an Owner, like `owner_kind`.
    pub owner: &'a str,
    pub owner_kind: &'a str,
}

/// Writes one Entry's row, in [`HEADER`]'s columns.
pub fn write_entry(out: &mut impl Write, entry: &CsvEntry) -> io::Result<()> {
    let modified = entry.modified_ns.map_or(String::new(), |ns| {
        chrono::DateTime::from_timestamp_nanos(ns).format("%Y-%m-%dT%H:%M:%SZ").to_string()
    });
    write_row(
        out,
        &[
            entry.path,
            entry.name,
            &entry.logical.to_string(),
            &entry.on_disk.to_string(),
            &modified,
            entry.category,
            entry.owner,
            entry.owner_kind,
        ],
    )
}

/// One RFC 4180 record: fields containing a comma, quote, CR or LF are quoted, quotes doubled.
/// A field a spreadsheet would read as a formula (starting with `=`, `+`, `-`, `@`, tab or CR)
/// gets a leading `'`, so a hostile file name cannot run when the CSV is opened.
fn write_row(out: &mut impl Write, fields: &[&str]) -> io::Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        let field = if field.starts_with(['=', '+', '-', '@', '\t', '\r']) {
            format!("'{field}")
        } else {
            field.to_string()
        };
        if field.contains([',', '"', '\r', '\n']) {
            write!(out, "\"{}\"", field.replace('"', "\"\""))?;
        } else {
            out.write_all(field.as_bytes())?;
        }
    }
    out.write_all(b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryMeta, SyncState};
    use std::path::Path;

    #[test]
    fn modified_is_written_from_the_index_nanoseconds() {
        let mut index = Index::new();
        let vol = index.add_volume(
            Path::new("/w"),
            "ext4",
            "uuid:w",
            EntryMeta {
                is_dir: true,
                ..Default::default()
            },
            SyncState::Walk,
        );
        let root = index.volume(vol).unwrap().root;
        // 2026-10-10T15:33:33Z
        let f = index.insert(
            root,
            "a.txt",
            EntryMeta {
                modified: 1_791_646_413_000_000_000,
                ..Default::default()
            },
        );
        let g = index.insert(root, "unknown", EntryMeta::default());
        index.set_owners(root, vec![(root, crate::Owner::new(crate::OwnerKind::Project, "w", ".git"))]);
        let mut out = Vec::new();
        write_csv(&mut out, &index, &Filter::compute(&index, None), &CategoryMap::builtin(), &[f, g]).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(",2026-10-10T15:33:33Z,"), "{text}");
        let unknown = index.path(g);
        assert!(
            text.contains(&format!("{},unknown,0,0,,Other,w,Project", unknown.display())),
            "{text}"
        );
    }

    fn row(fields: &[&str]) -> String {
        let mut out = Vec::new();
        write_row(&mut out, fields).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_fields_are_not_quoted() {
        assert_eq!(row(&["a", "b c", ""]), "a,b c,\r\n");
    }

    #[test]
    fn formula_looking_fields_get_a_leading_quote() {
        assert_eq!(
            row(&["=1+1", "+a", "-a", "@a", "\ta", "\ra,b", "a=b"]),
            "'=1+1,'+a,'-a,'@a,'\ta,\"'\ra,b\",a=b\r\n"
        );
    }

    #[test]
    fn commas_quotes_and_newlines_are_escaped() {
        assert_eq!(
            row(&["a,b", "say \"hi\"", "x\ny", "x\r\ny"]),
            "\"a,b\",\"say \"\"hi\"\"\",\"x\ny\",\"x\r\ny\"\r\n"
        );
    }
}
