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
        let modified = chrono::DateTime::from_timestamp(index.modified(id), 0)
            .filter(|_| index.modified(id) != 0)
            .map_or(String::new(), |t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
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
