//! CSV export of a list of Entries, shared by the app and (later) the CLI.

use crate::{CategoryMap, EntryId, Filter, Index};
use std::io::{self, Write};

/// Writes one CSV row per live Entry of `ids`: path, name, Logical Size, On-disk Size, modified
/// time (UTC, ISO 8601; empty when unknown) and Category (empty for folders). Sizes are Filtered
/// Sizes under `filter`, as the Explorer shows them.
pub fn write_csv(out: &mut impl Write, index: &Index, filter: &Filter, categories: &CategoryMap, ids: &[EntryId]) -> io::Result<()> {
    out.write_all(b"path,name,Logical Size,On-disk Size,modified,Category\r\n")?;
    for &id in ids.iter().filter(|&&id| index.is_live(id)) {
        let totals = filter.totals(index, id);
        let name = index.name(id);
        // The Index keeps nanoseconds since the Unix epoch; 0 is unknown.
        let modified = match index.modified(id) {
            0 => String::new(),
            ns => chrono::DateTime::from_timestamp_nanos(ns).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        };
        let category = if index.is_dir(id) { "" } else { categories.of_name(name).name() };
        let path = index.path(id);
        write_row(
            out,
            &[
                &path.to_string_lossy(),
                name,
                &totals.logical.to_string(),
                &totals.on_disk.to_string(),
                &modified,
                category,
            ],
        )?;
    }
    Ok(())
}

/// One RFC 4180 record: fields containing a comma, quote, CR or LF are quoted, quotes doubled.
fn write_row(out: &mut impl Write, fields: &[&str]) -> io::Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
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
        let mut out = Vec::new();
        write_csv(&mut out, &index, &Filter::compute(&index, None), &CategoryMap::builtin(), &[f, g]).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(",2026-10-10T15:33:33Z,"), "{text}");
        assert!(text.contains("/w/unknown,unknown,0,0,,"), "{text}");
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
    fn commas_quotes_and_newlines_are_escaped() {
        assert_eq!(
            row(&["a,b", "say \"hi\"", "x\ny", "x\r\ny"]),
            "\"a,b\",\"say \"\"hi\"\"\",\"x\ny\",\"x\r\ny\"\r\n"
        );
    }
}
