use chrono::{Local, TimeZone};
use ve_core::{Candidate, CategoryMap, Query, QueryContext};

fn context() -> QueryContext {
    QueryContext::new(CategoryMap::builtin(), Local.with_ymd_and_hms(2025, 3, 14, 12, 0, 0).unwrap())
}

fn file(name: &str) -> Candidate<'_> {
    Candidate {
        name,
        path: None,
        is_dir: false,
        logical: 0,
        modified: 0,
        ..Default::default()
    }
}

fn matches(query: &str, candidate: &Candidate) -> bool {
    Query::parse(query, &context()).unwrap().matches(candidate)
}

#[test]
fn plain_text_matches_part_of_a_name_ignoring_case() {
    assert!(matches("report", &file("Annual-REPORT-2024.pdf")));
    assert!(!matches("report", &file("summary.pdf")));
}

#[test]
fn space_separated_terms_must_all_match() {
    assert!(matches("tax 2024", &file("tax-return-2024.pdf")));
    assert!(!matches("tax 2024", &file("tax-return-2023.pdf")));
}

#[test]
fn bar_matches_either_side_and_binds_tighter_than_space() {
    assert!(matches("jpg|png", &file("cat.png")));
    assert!(!matches("jpg|png", &file("cat.gif")));
    // `cat dog|bird` means cat AND (dog OR bird).
    assert!(matches("cat dog|bird", &file("cat-bird.txt")));
    assert!(!matches("cat dog|bird", &file("dog.txt")));
}

#[test]
fn exclamation_excludes_matching_names() {
    assert!(matches("!tmp", &file("notes.txt")));
    assert!(!matches("!tmp", &file("notes.tmp")));
    assert!(matches("notes !backup", &file("notes.txt")));
    assert!(!matches("notes !backup", &file("notes-backup.txt")));
}

#[test]
fn quotes_keep_spaces_inside_one_term() {
    assert!(matches("\"my file\"", &file("my file.txt")));
    assert!(!matches("\"my file\"", &file("file my.txt")));
}

#[test]
fn empty_query_matches_everything() {
    assert!(matches("", &file("anything")));
    assert!(matches("   ", &file("anything")));
}

fn at(path: &str) -> Candidate<'_> {
    let name = path.rsplit('/').next().unwrap();
    Candidate {
        name,
        path: Some(path),
        is_dir: false,
        logical: 0,
        modified: 0,
        ..Default::default()
    }
}

#[test]
fn wildcards_match_the_whole_name() {
    assert!(matches("*.log", &file("Server.LOG")));
    assert!(!matches("*.log", &file("server.log.gz")));
    assert!(matches("img_????.jpg", &file("IMG_0042.jpg")));
    assert!(!matches("img_????.jpg", &file("IMG_00042.jpg")));
}

#[test]
fn wildcards_ignore_case_for_ascii_and_unicode_names() {
    assert!(matches("img_*.jpg", &file("IMG_0042.JPG")));
    assert!(matches("IMG_????.JPG", &file("img_0042.jpg")));
    assert!(!matches("img_*.png", &file("IMG_0042.JPG")));
    // `?` is one character, however many bytes it takes.
    assert!(matches("caf?.txt", &file("café.txt")));
    assert!(matches("?.txt", &file("日.txt")));
    assert!(!matches("??.txt", &file("日.txt")));
    // Backtracking after a partial multi-byte match.
    assert!(matches("*é", &file("éaé")));
    assert!(matches("é*", &file("ÉCOLE")));
    assert!(!matches("*é", &file("éa")));
}

#[test]
fn a_slash_or_backslash_matches_against_the_full_path() {
    let query = Query::parse("node_modules/", &context()).unwrap();
    assert!(query.needs_path());
    assert!(query.matches(&at("/home/me/app/node_modules/react/index.js")));
    assert!(!query.matches(&at("/home/me/app/src/node_modules.txt")));
    assert!(matches("users\\me", &at("C:/Users/Me/notes.txt")));
    assert!(matches("/home/*/downloads/*.iso", &at("/home/me/Downloads/arch.iso")));
    // `*` also spans folder separators, as in Everything.
    assert!(matches("/home/*/downloads/*.iso", &at("/home/me/Downloads/old/arch.iso")));
    assert!(!matches("/home/*/downloads/*.iso", &at("/home/me/Downloads/arch.iso.part")));
}

#[test]
fn plain_terms_do_not_need_the_path() {
    assert!(!Query::parse("report *.pdf", &context()).unwrap().needs_path());
}

fn folder(name: &str) -> Candidate<'_> {
    Candidate {
        name,
        path: None,
        is_dir: true,
        logical: 0,
        modified: 0,
        ..Default::default()
    }
}

#[test]
fn ext_matches_any_listed_extension_of_files_only() {
    assert!(matches("ext:mp4", &file("Holiday.MP4")));
    assert!(matches("ext:mp4;mkv", &file("film.mkv")));
    assert!(!matches("ext:mp4", &file("mp4.txt")));
    assert!(!matches("ext:mp4", &folder("archive.mp4")));
}

#[test]
fn type_matches_files_of_a_category() {
    assert!(matches("type:video", &file("clip.webm")));
    assert!(!matches("type:video", &file("song.flac")));
    assert!(matches("type:other", &file("README")));
    assert!(!matches("type:image", &folder("photos.png")));
}

#[test]
fn type_follows_category_overrides() {
    let categories = CategoryMap::from_toml("[extensions]\nblend = \"Image\"\nflac = \"Other\"\n").unwrap();
    let context = QueryContext::new(categories, context().now);
    assert!(Query::parse("type:image", &context).unwrap().matches(&file("scene.blend")));
    assert!(Query::parse("type:other", &context).unwrap().matches(&file("song.flac")));
}

#[test]
fn unknown_prefixes_are_plain_text() {
    // Drive letters look like modifiers but are text.
    assert!(matches("c:\\users", &at("C:/Users/me")));
    assert!(matches("note:", &file("note: draft.txt")));
}

#[test]
fn quoted_modifier_names_are_plain_text() {
    assert!(matches("\"ext:\"", &file("ext: notes")));
}

fn sized(logical: u64) -> Candidate<'static> {
    Candidate {
        name: "blob.bin",
        path: None,
        is_dir: false,
        logical,
        modified: 0,
        ..Default::default()
    }
}

const MB: u64 = 1024 * 1024;

#[test]
fn size_compares_logical_size_with_binary_units() {
    assert!(matches("size:>1gb", &sized(1024 * MB + 1)));
    assert!(!matches("size:>1gb", &sized(1024 * MB)));
    assert!(matches("size:>=1gb", &sized(1024 * MB)));
    assert!(matches("size:<10kb", &sized(10 * 1024 - 1)));
    assert!(!matches("size:<10kb", &sized(10 * 1024)));
    assert!(matches("size:<=1.5mb", &sized(MB + MB / 2)));
    assert!(matches("size:2mb", &sized(2 * MB)));
    assert!(!matches("size:2mb", &sized(2 * MB + 1)));
}

#[test]
fn size_range_includes_both_ends() {
    assert!(matches("size:1mb..2mb", &sized(MB)));
    assert!(matches("size:1mb..2mb", &sized(2 * MB)));
    assert!(!matches("size:1mb..2mb", &sized(2 * MB + 1)));
}

#[test]
fn size_never_matches_folders() {
    assert!(!matches("size:<1kb", &folder("empty")));
}

fn modified_at(y: i32, mo: u32, d: u32, h: u32) -> Candidate<'static> {
    let modified = Local.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap().timestamp_nanos_opt().unwrap();
    Candidate {
        name: "doc.txt",
        path: None,
        is_dir: false,
        logical: 0,
        modified,
        ..Default::default()
    }
}

#[test]
fn dm_matches_a_year_month_or_day_in_local_time() {
    assert!(matches("dm:2025", &modified_at(2025, 1, 1, 0)));
    assert!(matches("dm:2025", &modified_at(2025, 12, 31, 23)));
    assert!(!matches("dm:2025", &modified_at(2024, 12, 31, 23)));
    assert!(matches("dm:2024-02", &modified_at(2024, 2, 29, 10)));
    assert!(!matches("dm:2024-02", &modified_at(2024, 3, 1, 0)));
    assert!(matches("dm:2024-02-29", &modified_at(2024, 2, 29, 23)));
}

#[test]
fn dm_comparisons_and_ranges() {
    assert!(matches("dm:>2024", &modified_at(2025, 1, 1, 0)));
    assert!(!matches("dm:>2024", &modified_at(2024, 6, 1, 0)));
    assert!(matches("dm:>=2024", &modified_at(2024, 6, 1, 0)));
    assert!(matches("dm:<2024-06", &modified_at(2024, 5, 31, 23)));
    assert!(!matches("dm:<2024-06", &modified_at(2024, 6, 1, 0)));
    assert!(matches("dm:<=2024-06", &modified_at(2024, 6, 30, 23)));
    assert!(matches("dm:2023..2024-03", &modified_at(2024, 3, 31, 23)));
    assert!(!matches("dm:2023..2024-03", &modified_at(2024, 4, 1, 0)));
}

#[test]
fn dm_today_and_yesterday_follow_the_context_clock() {
    // The context clock reads 2025-03-14 12:00 local time.
    assert!(matches("dm:today", &modified_at(2025, 3, 14, 1)));
    assert!(!matches("dm:today", &modified_at(2025, 3, 13, 23)));
    assert!(matches("dm:yesterday", &modified_at(2025, 3, 13, 23)));
}

#[test]
fn dm_never_matches_folders() {
    let mut candidate = modified_at(2025, 3, 14, 1);
    candidate.is_dir = true;
    assert!(!Query::parse("dm:2025", &context()).unwrap().matches(&candidate));
}

#[test]
fn regex_matches_names_ignoring_case() {
    assert!(matches("regex:^img_\\d{4}\\.jpe?g$", &file("IMG_2024.JPEG")));
    assert!(!matches("regex:^img_\\d{4}\\.jpe?g$", &file("my_img_2024.jpg")));
}

#[test]
fn invalid_modifier_values_are_errors_naming_the_modifier() {
    for query in [
        "size:huge",
        "size:5zb",
        "dm:2025-13",
        // The last representable year: its end lies past the calendar's range.
        "dm:262142",
        "dm:soon",
        "type:movies",
        "regex:(",
        "ext:",
    ] {
        let error = Query::parse(query, &context()).expect_err(query);
        let modifier = query.split(':').next().unwrap();
        assert!(error.0.contains(modifier), "{query}: {error}");
    }
}

#[test]
fn exclusions_are_the_stand_alone_negated_terms() {
    let query = Query::parse("node_modules !*.map !ext:log|ext:tmp", &context()).unwrap();
    assert!(query.passes_exclusions(&file("index.js")));
    assert!(!query.passes_exclusions(&file("index.js.map")));
    // A negation inside an OR group is not a stand-alone exclusion.
    assert!(query.passes_exclusions(&file("debug.log")));
    assert!(Query::parse("report", &context()).unwrap().passes_exclusions(&file("anything")));
}

#[test]
fn case_matches_letters_exactly() {
    assert!(matches("case:Report", &file("Annual-Report.pdf")));
    assert!(!matches("case:Report", &file("annual-report.pdf")));
    // Wildcards still cover the whole name, now case-sensitively.
    assert!(matches("case:R*.pdf", &file("Report.pdf")));
    assert!(!matches("case:R*.pdf", &file("report.pdf")));
    assert!(!matches("case:r?port.PDF", &file("report.pdf")));
    // Only the term it prefixes is case-sensitive.
    assert!(matches("case:Report PDF", &file("Report.pdf")));
}

#[test]
fn case_combines_with_negation_and_bar() {
    assert!(matches("!case:Report", &file("report.txt")));
    assert!(!matches("!case:Report", &file("Report.txt")));
    assert!(matches("case:Foo|case:Bar", &file("xBar")));
    assert!(!matches("case:Foo|case:Bar", &file("xbar")));
    assert!(matches("case:Foo|bar", &file("xBAR")));
}

#[test]
fn case_applies_to_paths() {
    let query = Query::parse("case:/Docs/", &context()).unwrap();
    assert!(query.matches(&at("/home/Docs/a.txt")));
    assert!(!query.matches(&at("/home/docs/a.txt")));
}

#[test]
fn wholeword_needs_word_boundaries_at_punctuation() {
    assert!(matches("wholeword:report", &file("annual report.pdf")));
    assert!(matches("wholeword:report", &file("Annual-Report_2024.pdf")));
    assert!(matches("wholeword:report", &file("report")));
    assert!(matches("wholeword:report", &file("(report)")));
    assert!(!matches("wholeword:report", &file("reports.pdf")));
    assert!(!matches("wholeword:report", &file("preport.pdf")));
    assert!(!matches("wholeword:2024", &file("v20241.txt")));
    // A later whole occurrence still counts.
    assert!(matches("wholeword:cat", &file("category cat.txt")));
}

#[test]
fn wholeword_with_wildcards_covers_a_whole_word_run() {
    assert!(matches("wholeword:re*t", &file("annual report.pdf")));
    assert!(!matches("wholeword:re*t", &file("preport.pdf")));
    assert!(matches("wholeword:c?t", &file("a-cat.txt")));
    assert!(!matches("wholeword:c?t", &file("cart.txt")));
}

#[test]
fn wholeword_combines_with_case_negation_and_bar() {
    assert!(matches("case:wholeword:Cat", &file("The Cat.txt")));
    assert!(!matches("case:wholeword:Cat", &file("the cat.txt")));
    assert!(matches("wholeword:Cat case:wholeword:cat", &file("cat Cat")));
    assert!(matches("!wholeword:cat", &file("category")));
    assert!(!matches("!wholeword:cat", &file("a cat")));
    assert!(matches("wholeword:cat|wholeword:dog", &file("hot-dog")));
    assert!(!matches("wholeword:cat|wholeword:dog", &file("dogs")));
}

#[test]
fn case_and_wholeword_need_a_text_term() {
    for bad in ["case:", "wholeword:", "case:ext:png", "wholeword:size:>1"] {
        assert!(Query::parse(bad, &context()).is_err(), "{bad}");
    }
    // Quoted, the colon is part of the name.
    assert!(matches("\"case:x\"", &file("case:x")));
}

fn placed<'a>(path: &'a str, depth: u32, children: &[&'a str]) -> Candidate<'a> {
    Candidate {
        name: path.rsplit('/').next().unwrap(),
        path: Some(path),
        is_dir: !children.is_empty() || path.ends_with("dir"),
        depth,
        children: children.to_vec(),
        ..Default::default()
    }
}

#[test]
fn parent_matches_entries_directly_inside_a_folder() {
    let query = Query::parse("parent:/home/me/Docs", &context()).unwrap();
    assert!(query.needs_path());
    assert!(query.matches(&at("/home/me/docs/a.txt")));
    assert!(!query.matches(&at("/home/me/docs/sub/a.txt")));
    assert!(!query.matches(&at("/home/me/docs2/a.txt")));
    assert!(!query.matches(&at("/home/me/Docs")));
    assert!(matches("parent:/home/me/Docs/", &at("/home/me/docs/a.txt")));
    assert!(matches("parent:C:\\Users", &at("C:/Users/me")));
    assert!(matches("parent:/ !x", &at("/home")));
    assert!(matches("parent:\"/my docs\"", &at("/my docs/a")));
    assert!(matches("parent:/a|parent:/b", &at("/b/c")));
    // A Location root is inside nothing.
    assert!(!matches("parent:/", &at("/")));
    assert!(!matches("parent:C:/", &at("C:/")));
    assert!(Query::parse("parent:", &context()).is_err());
}

#[test]
fn depth_counts_levels_below_the_location_root() {
    let at_depth = |d| placed("/r/x", d, &[]);
    assert!(Query::parse("depth:2", &context()).unwrap().needs_depth());
    assert!(matches("depth:2", &at_depth(2)));
    assert!(!matches("depth:2", &at_depth(3)));
    assert!(matches("depth:>1", &at_depth(2)) && !matches("depth:>1", &at_depth(1)));
    assert!(matches("depth:>=1", &at_depth(1)) && !matches("depth:>=1", &at_depth(0)));
    assert!(matches("depth:<2", &at_depth(1)) && !matches("depth:<2", &at_depth(2)));
    assert!(!matches("depth:<0", &at_depth(0)));
    assert!(matches("depth:<=2", &at_depth(2)) && !matches("depth:<=2", &at_depth(3)));
    assert!(matches("depth:1..3", &at_depth(3)) && !matches("depth:1..3", &at_depth(4)));
    assert!(matches("!depth:1..3 log", &placed("/r/log", 4, &[])));
    for bad in ["depth:", "depth:x", "depth:-1"] {
        assert!(Query::parse(bad, &context()).is_err(), "{bad}");
    }
}

#[test]
fn child_matches_folders_with_a_direct_child_by_name() {
    let query = Query::parse("child:.git", &context()).unwrap();
    assert!(query.needs_children());
    assert!(query.matches(&placed("/r/proj", 1, &["src", ".GIT"])));
    assert!(!query.matches(&placed("/r/proj", 1, &["src"])));
    // Files never have children, so a negated `child:` keeps them.
    assert!(!query.matches(&file("proj")));
    assert!(matches("!child:.git", &file("proj")));
    assert!(matches("child:*.toml", &placed("/r/p", 1, &["Cargo.toml"])));
    assert!(!matches("child:*.toml", &placed("/r/p", 1, &["Cargo.toml.bak"])));
    assert!(matches("child:readme|child:license", &placed("/r/p", 1, &["LICENSE"])));
    assert!(Query::parse("child:", &context()).is_err());
    assert!(Query::parse("child:a/b", &context()).is_err());
}

#[test]
fn empty_matches_folders_without_children() {
    let query = Query::parse("empty:", &context()).unwrap();
    assert!(query.needs_children());
    assert!(query.matches(&placed("/r/emptydir", 1, &[])));
    assert!(!query.matches(&placed("/r/full", 1, &["a"])));
    assert!(!query.matches(&file("a.txt")));
    assert!(matches("!empty:", &placed("/r/full", 1, &["a"])));
    assert!(Query::parse("empty:x", &context()).is_err());
}

#[test]
fn tree_modifiers_combine_with_each_other_and_with_text() {
    let candidate = placed("/r/build/emptydir", 2, &[]);
    assert!(matches("empty: depth:2 parent:/r/build dir", &candidate));
    assert!(!matches("empty: depth:3", &candidate));
    assert!(matches("depth:5|empty:", &candidate));
    assert!(!Query::parse("build", &context()).unwrap().needs_children());
}

#[test]
fn tree_modifiers_filter_a_real_index() {
    use std::path::Path;
    use ve_core::{EntryMeta, Filter, Index, SyncState};
    let dir = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    let mut index = Index::new();
    let vol = index.add_volume(Path::new("/m/loc"), "ext4", "loc", dir, SyncState::Walk);
    let root = index.volume(vol).unwrap().root;
    let proj = index.insert(root, "proj", dir);
    let git = index.insert(proj, ".git", dir);
    let hollow = index.insert(proj, "hollow", dir);
    let file = index.insert(proj, "a.txt", EntryMeta::default());
    let nested = index.insert(git, "HEAD", EntryMeta::default());
    let matched = |q: &str| {
        let query = Query::parse(q, &context()).unwrap();
        let filter = Filter::compute(&index, Some(&query));
        [proj, git, hollow, file, nested].map(|id| filter.is_matched(id))
    };
    assert_eq!(matched("depth:2"), [false, true, true, true, false]);
    assert_eq!(matched("child:.git"), [true, false, false, false, false]);
    assert_eq!(matched("empty:"), [false, false, true, false, false]);
    assert_eq!(matched("parent:/m/loc/proj"), [false, true, true, true, false]);
}

#[test]
fn path_queries_see_each_entrys_own_path_across_siblings_folders_and_roots() {
    use std::path::Path;
    use ve_core::{EntryMeta, Filter, Index, SyncState};
    let dir = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    let mut index = Index::new();
    let vol = index.add_volume(Path::new("/m/loc"), "ext4", "loc", dir, SyncState::Walk);
    let root = index.volume(vol).unwrap().root;
    let a = index.insert(root, "a", dir);
    let b = index.insert(root, "b", dir);
    let ax = index.insert(a, "x.txt", EntryMeta::default());
    let ay = index.insert(a, "y.txt", EntryMeta::default());
    let bx = index.insert(b, "x.txt", EntryMeta::default());
    let all = [root, a, b, ax, ay, bx];
    let matched = |q: &str| {
        let query = Query::parse(q, &context()).unwrap();
        let filter = Filter::compute(&index, Some(&query));
        all.map(|id| filter.is_matched(id))
    };
    // Siblings share a cached parent path; the entry in another folder must not reuse it.
    assert_eq!(matched("/m/loc/a/x.txt"), [false, false, false, true, false, false]);
    assert_eq!(matched("/m/loc/b/x.txt"), [false, false, false, false, false, true]);
    assert_eq!(matched("/m/loc/a/?.txt"), [false, false, false, true, true, false]);
    // The root has no parent: its path is its own.
    assert_eq!(matched("/m/l?c"), [true, false, false, false, false, false]);
}

#[test]
fn dupe_matches_files_the_context_lists_as_having_a_duplicate() {
    let mut set = ve_core::BitSet::new(4);
    set.insert(2);
    let context = context().with_duplicates(std::sync::Arc::new(set));
    let query = Query::parse("dupe:", &context).unwrap();
    assert!(query.needs_duplicates());
    assert!(query.has_duplicate(2) && !query.has_duplicate(1));
    assert!(query.matches(&Candidate {
        name: "a",
        duplicate: true,
        ..Default::default()
    }));
    assert!(!query.matches(&Candidate {
        name: "a",
        ..Default::default()
    }));
    assert!(!query.matches(&Candidate {
        name: "a",
        is_dir: true,
        duplicate: true,
        ..Default::default()
    }));
    assert!(Query::parse("dupe:x", &context).is_err());
}

#[test]
fn owner_and_ownerkind_follow_the_nearest_owner_root_for_files_and_folders() {
    use std::path::Path;
    use ve_core::{EntryMeta, Filter, Index, Owner, OwnerKind, SyncState};
    let dir = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    let mut index = Index::new();
    let vol = index.add_volume(Path::new("/m/loc"), "ext4", "loc", dir, SyncState::Walk);
    let root = index.volume(vol).unwrap().root;
    let game = index.insert(root, "System Shock", dir);
    let save = index.insert(game, "save.dat", EntryMeta::default());
    let mods = index.insert(game, "mods", dir);
    let mod_src = index.insert(mods, "main.rs", EntryMeta::default());
    let loose = index.insert(root, "loose.txt", EntryMeta::default());
    index.set_owners(
        root,
        vec![
            (game, Owner::new(OwnerKind::Game, "System Shock", "Steam")),
            (mods, Owner::new(OwnerKind::Project, "mods", "Cargo.toml")),
        ],
    );
    let all = [root, game, save, mods, mod_src, loose];
    let matched = |q: &str| {
        let query = Query::parse(q, &context()).unwrap();
        let filter = Filter::compute(&index, Some(&query));
        all.map(|id| filter.is_matched(id))
    };
    // Name substring, ignoring case; folders match too; the nearer root wins.
    assert_eq!(matched("owner:shock"), [false, true, true, false, false, false]);
    assert_eq!(matched("owner:\"system shock\""), [false, true, true, false, false, false]);
    assert_eq!(matched("owner:mods"), [false, false, false, true, true, false]);
    // The kind is a separate key, so a game called "System" is not the System kind.
    assert_eq!(matched("ownerkind:system"), [false, false, false, false, false, false]);
    assert_eq!(matched("ownerkind:game"), [false, true, true, false, false, false]);
    assert_eq!(matched("ownerkind:Project"), [false, false, false, true, true, false]);
    assert_eq!(matched("ownerkind:none"), [true, false, false, false, false, true]);
    assert_eq!(matched("!ownerkind:none"), [false, true, true, true, true, false]);
    assert!(Query::parse("owner:", &context()).is_err());
    assert!(Query::parse("ownerkind:package", &context()).is_err());
}

#[test]
fn dc_matches_like_dm_and_never_an_unknown_creation_time() {
    let created = |y, mo, d, h| {
        let created = Local.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap().timestamp_nanos_opt().unwrap();
        Candidate {
            name: "doc.txt",
            created,
            ..Default::default()
        }
    };
    assert!(matches("dc:2025", &created(2025, 6, 1, 0)));
    assert!(!matches("dc:2025", &created(2024, 12, 31, 23)));
    assert!(matches("dc:>2024", &created(2025, 1, 1, 0)));
    assert!(!matches(
        "dc:<2020",
        &Candidate {
            name: "doc.txt",
            ..Default::default()
        }
    ));
    assert!(!matches("dc:2025", &folder("dir")));
}
