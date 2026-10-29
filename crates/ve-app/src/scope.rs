//! Zooming into a folder is a Query clause, `"/path/to/folder/*"`, so the tree, flat list,
//! Treemap and Category breakdown all follow it and the Query can still be refined around it.

use ve_core::query::clauses;
use ve_core::{EntryId, Index};

/// The clause that keeps a Query to everything under `folder`, or None when no path clause can
/// (a Volume root that other Volumes are mounted under, such as `/`): the whole Index then.
pub fn term(index: &Index, folder: EntryId) -> Option<String> {
    let mut path = String::new();
    let mut chain = Vec::new();
    index.write_match_path(folder, &mut path, &mut chain);
    if !path.ends_with('/') {
        path.push('/');
    }
    let holds_another_volume = index.volumes().any(|(_, v)| {
        if v.root == folder {
            return false;
        }
        let mut other = String::new();
        index.write_match_path(v.root, &mut other, &mut chain);
        other.starts_with(&path)
    });
    if index.parent(folder).is_none() && holds_another_volume {
        return None;
    }
    // Quotes, `*` and `?` cannot be escaped in a Query; `?` matches each of them exactly once.
    let pattern: String = path.chars().map(|c| if matches!(c, '"' | '*' | '?') { '?' } else { c }).collect();
    Some(format!("\"{pattern}*\""))
}

/// Whether one clause of Query text is a folder scope as [`term`] writes it.
fn is_scope(clause: &ve_core::query::ClauseSource) -> bool {
    let [word] = clause.words.as_slice() else {
        return false;
    };
    !word.negated
        && word.modifier.is_none()
        && word
            .value
            .strip_suffix("/*")
            .is_some_and(|folder| !folder.contains('*') && (folder.is_empty() || folder.contains('/') || folder.ends_with(':')))
}

/// `query` with its folder scope replaced by `term`, or removed when `term` is None. The scope
/// goes first; every other clause is kept as typed.
pub fn set(query: &str, term: Option<&str>) -> String {
    term.into_iter()
        .chain(clauses(query).iter().filter(|c| !is_scope(c)).map(|c| c.source))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `query` with only its folder scope kept: the Escape step that drops the other terms.
pub fn keep_scope(query: &str) -> String {
    clauses(query)
        .iter()
        .filter(|c| is_scope(c))
        .map(|c| c.source)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::View;
    use ve_core::{CategoryMap, EntryMeta, Query, QueryContext, Remote};

    #[test]
    fn a_scoped_query_shows_the_folder_and_the_treemap_draws_it() {
        let mut index = Index::default();
        let volume = index.add_remote(&Remote::new("me@box", "/srv").unwrap());
        let root = index.volume(volume).unwrap().root;
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let file = EntryMeta {
            logical: 10,
            on_disk: 4096,
            ..Default::default()
        };
        let a = index.insert(root, "a \"q\"", dir);
        let b = index.insert(a, "b", dir);
        let x = index.insert(b, "x.rs", file);
        let outside = index.insert(root, "c.rs", file);
        let categories = CategoryMap::builtin();
        let view = |text: &str| {
            let query = Query::parse(text, &QueryContext::new(categories.clone(), chrono::Local::now())).unwrap();
            View::compute(&index, text, Some(&query), &categories, &mut crate::view::FileOrder::default())
        };
        let text = set("ext:rs", term(&index, a).as_deref());
        // `a` holds only `b`, yet the Treemap draws `a`: the folder zoomed into.
        let scoped = view(&set("", term(&index, a).as_deref()));
        assert_eq!(scoped.focus, Some(a));
        assert!(scoped.flat.contains(&b) && scoped.flat.contains(&x) && !scoped.flat.contains(&outside));
        // Refined around the scope: only `x.rs` matches, so the Treemap draws its folder.
        let refined = view(&text);
        assert_eq!((refined.flat.clone(), refined.focus), (vec![x], Some(b)));
    }

    #[test]
    fn replaces_only_the_scope_and_keeps_the_rest() {
        let scoped = set("ext:rs \"/home/me/a b/*\" !target", Some("\"/srv/*\""));
        assert_eq!(scoped, "\"/srv/*\" ext:rs !target");
        assert_eq!(set(&scoped, None), "ext:rs !target");
        // Other path terms are the user's own and stay.
        assert_eq!(set("/home/*.rs foo/", Some("\"/x/*\"")), "\"/x/*\" /home/*.rs foo/");
        // A scope inside an `|` clause or negated is not a scope.
        assert_eq!(set("\"/a/*\"|b !\"/c/*\"", None), "\"/a/*\"|b !\"/c/*\"");
        assert_eq!(set("", Some("\"/x/*\"")), "\"/x/*\"");
    }

    #[test]
    fn keep_scope_drops_everything_but_the_scope() {
        assert_eq!(keep_scope("ext:rs \"/srv/a/*\" foo"), "\"/srv/a/*\"");
        assert_eq!(keep_scope("ext:rs foo"), "");
        assert_eq!(keep_scope("\"/srv/a/*\""), "\"/srv/a/*\"");
        assert_eq!(keep_scope(""), "");
    }
}
