//! `launch.match` through the Engine handle: the Launcher's matches of Entry names in the Index
//! and of installed programs.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use ve_core::CategoryMap;
#[cfg(unix)]
use ve_engine::apps::Sources;
use ve_engine::launch::{Kind, Match};
#[cfg(unix)]
use ve_engine::launch::{Launch, Start};
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

fn volume(identity: &str, mount: &Path) -> VolumeInfo {
    VolumeInfo {
        mount: mount.to_path_buf(),
        identity: identity.into(),
        fs_type: "ext4".into(),
        label: identity.into(),
        removable: true,
        network: false,
        read_only: false,
    }
}

/// An Engine over `mounted` that lists no installed programs.
fn start(data: &Path, mounted: Vec<VolumeInfo>) -> Engine {
    let engine = Engine::start(
        Config {
            index_path: Some(data.join("index.bin")),
            mounted: mounted.clone(),
            volumes: mounted,
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    );
    engine.apps.set_sources(None);
    engine
}

#[cfg(unix)]
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A pacman package installed at `when`, with these files.
#[cfg(unix)]
fn package(root: &Path, name: &str, when: i64, files: &[&str]) {
    let entry = root.join(format!("var/lib/pacman/local/{name}-1-1"));
    write(&entry.join("desc"), &format!("%NAME%\n{name}\n\n%INSTALLDATE%\n{when}\n"));
    write(&entry.join("files"), &format!("%FILES%\n{}\n", files.join("\n")));
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn idle(engine: &Engine) -> bool {
    !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle)
}

/// A file (or, ending in `/`, a folder) modified `days` days ago.
fn make(path: &Path, days: u64) {
    if path.as_os_str().to_string_lossy().ends_with('/') {
        fs::create_dir_all(path).unwrap();
    } else {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }
    let when = SystemTime::now() - Duration::from_secs(days * 86_400);
    open_to_touch(path).set_modified(when).unwrap();
}

/// A handle that may set `path`'s times. Windows opens a folder only with backup semantics.
fn open_to_touch(path: &Path) -> fs::File {
    let mut options = fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    #[cfg(not(windows))]
    options.read(true);
    options.open(path).unwrap()
}

fn names(matches: &[Match]) -> Vec<&str> {
    matches.iter().map(|m| m.name.as_str()).collect()
}

#[test]
fn matches_rank_name_starts_then_word_starts_then_the_rest_newest_first_and_mark_offline_volumes() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (here, away) = (fixture.path().join("here"), fixture.path().join("away"));
    make(&here.join("notes/report.txt"), 30);
    make(&here.join("Reports/"), 2);
    make(&here.join("notes/annual-report.pdf"), 5);
    make(&here.join("notes/MyReport.doc"), 20);
    make(&here.join("notes/prereport.log"), 1);
    make(&here.join("notes/unrelated.txt"), 1);
    make(&away.join("old-report.md"), 10);
    let (here, away) = (volume("uuid:here", &here), volume("uuid:away", &away));

    // Both Volumes are indexed; then `away` is unplugged and the app started again.
    let engine = start(data.path(), vec![here.clone(), away]);
    wait_for("the first Sync", || {
        idle(&engine)
            && engine
                .shared
                .index
                .read()
                .volumes()
                .filter(|(_, v)| v.last_synced.is_some())
                .count()
                == 2
    });
    engine.shutdown();
    drop(engine);
    let engine = start(data.path(), vec![here]);
    wait_for("the saved Index", || idle(&engine));

    let found = engine.launch_match("REPORT").unwrap();
    assert_eq!(
        names(&found),
        [
            // The name starts with the text, newest first.
            "Reports",
            "report.txt",
            // A word in the name starts with it (after a separator or a capital), newest first.
            "annual-report.pdf",
            "old-report.md",
            "MyReport.doc",
            // Anywhere else in the name.
            "prereport.log",
        ]
    );
    let by_name = |name: &str| found.iter().find(|m| m.name == name).unwrap();
    assert_eq!(by_name("Reports").kind, Kind::Folder);
    assert_eq!(by_name("report.txt").kind, Kind::File);
    assert_eq!(by_name("report.txt").path, fixture.path().join("here/notes/report.txt"));
    assert!(by_name("old-report.md").offline, "away is not mounted");
    assert!(found.iter().filter(|m| m.name != "old-report.md").all(|m| !m.offline));
    engine.shutdown();
}

#[test]
fn matches_are_capped_at_fifty_and_empty_text_matches_nothing() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mount = fixture.path().join("volume");
    for n in 0..60 {
        // Higher numbers are newer.
        make(&mount.join(format!("clip-{n:02}.mp4")), 100 - n);
    }
    let engine = start(data.path(), vec![volume("uuid:clips", &mount)]);
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());

    let found = engine.launch_match("clip-").unwrap();
    assert_eq!(found.len(), 50);
    // The fifty newest: clip-59 down to clip-10.
    assert_eq!(found.first().unwrap().name, "clip-59.mp4");
    assert_eq!(found.last().unwrap().name, "clip-10.mp4");

    assert_eq!(engine.launch_match("").unwrap(), []);
    assert_eq!(engine.launch_match("   ").unwrap(), []);
    engine.shutdown();
}

#[test]
#[cfg(unix)]
fn programs_with_a_launch_command_rank_among_entries_by_quality_then_recency() {
    let (data, fixture, system) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let mount = fixture.path().join("volume");
    make(&mount.join("viewer-notes.txt"), 1);
    make(&mount.join("Image Viewer.png"), 3000);
    make(&mount.join("preview.log"), 1);
    let engine = start(data.path(), vec![volume("uuid:v", &mount)]);
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());

    let (root, home) = (system.path().join("root"), system.path().join("home"));
    let day = |ago: i64| SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64 - ago * 86_400;
    // A package with a desktop entry a menu shows: launched by it.
    let desktop = root.join("usr/share/applications/viewer.desktop");
    write(&desktop, "[Desktop Entry]\nType=Application\nName=Viewer\nExec=viewer %F\n");
    package(
        &root,
        "viewer",
        day(2),
        &["usr/bin/viewer", "usr/share/applications/viewer.desktop"],
    );
    // Hidden, and terminal-only, desktop entries do not make a package launchable.
    write(
        &root.join("usr/share/applications/viewer-helper.desktop"),
        "[Desktop Entry]\nType=Application\nExec=h\nNoDisplay=true\n",
    );
    package(&root, "viewer-helper", day(1), &["usr/share/applications/viewer-helper.desktop"]);
    write(
        &root.join("usr/share/applications/viewtop.desktop"),
        "[Desktop Entry]\nType=Application\nExec=viewtop\nTerminal=true\n",
    );
    package(&root, "viewtop", day(1), &["usr/share/applications/viewtop.desktop"]);
    // A library: no desktop entry.
    package(&root, "libviewer", day(1), &["usr/lib/libviewer.so"]);
    // An /opt folder: no launch command is known.
    fs::create_dir_all(root.join("opt/viewer-pro")).unwrap();
    // A Flatpak, named by its desktop file.
    let flatpak = root.join("var/lib/flatpak/app/org.example.ImageViewer");
    write(
        &flatpak.join("x86_64/stable/active/export/share/applications/org.example.ImageViewer.desktop"),
        "[Desktop Entry]\nName=Image Viewer\n",
    );
    std::os::unix::fs::symlink("x86_64/stable", flatpak.join("current")).unwrap();
    // A Steam game.
    let steamapps = home.join(".local/share/Steam/steamapps");
    write(
        &steamapps.join("appmanifest_70.acf"),
        "\"AppState\"\n{\n\t\"appid\"\t\t\"70\"\n\t\"name\"\t\t\"Viewers Quest\"\n\t\"installdir\"\t\t\"VQ\"\n\t\"LastUpdated\"\t\t\"1\"\n}\n",
    );
    fs::create_dir_all(steamapps.join("common/VQ")).unwrap();
    engine.apps.set_sources(Some(Sources::new(root.clone(), Some(home))));

    let found = engine.launch_match("view").unwrap();
    let summary: Vec<_> = found.iter().map(|m| (m.name.as_str(), m.kind)).collect();
    assert_eq!(
        summary,
        [
            // The name starts with the text: programs first, most recent first (the package
            // from two days ago, the game from 1970), then the file from yesterday.
            ("viewer", Kind::Program),
            ("Viewers Quest", Kind::Program),
            ("viewer-notes.txt", Kind::File),
            // A word starts with it.
            ("Image Viewer", Kind::Program),
            ("Image Viewer.png", Kind::File),
            // Anywhere.
            ("preview.log", Kind::File),
        ]
    );
    let by_name = |name: &str| found.iter().find(|m| m.name == name).unwrap();
    assert_eq!(by_name("viewer").launch, Some(Launch::DesktopEntry(desktop.clone())));
    assert_eq!(by_name("viewer").path, desktop);
    assert_eq!(
        by_name("Image Viewer").launch,
        Some(Launch::Flatpak("org.example.ImageViewer".into()))
    );
    assert_eq!(by_name("Viewers Quest").launch, Some(Launch::Steam("70".into())));
    assert!(found.iter().filter(|m| m.kind != Kind::Program).all(|m| m.launch.is_none()));

    // Each kind's launch command.
    let argv = |launch: &Launch| match launch.start().unwrap() {
        Start::Run { argv, .. } => argv.into_iter().map(|a| a.into_string().unwrap()).collect::<Vec<_>>(),
        Start::Open(url) => vec![url],
    };
    assert_eq!(argv(by_name("viewer").launch.as_ref().unwrap()), ["viewer"]);
    assert_eq!(
        argv(by_name("Image Viewer").launch.as_ref().unwrap()),
        ["flatpak", "run", "org.example.ImageViewer"]
    );
    assert_eq!(argv(by_name("Viewers Quest").launch.as_ref().unwrap()), ["steam://rungameid/70"]);
    engine.shutdown();
}
