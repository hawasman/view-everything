//! `apps.list` through the Engine handle: programs read from fixture sources, sized from the
//! Owner roots a Sync of a fixture Location found.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::CategoryMap;
use ve_engine::apps::{App, Sources};
use ve_engine::{Config, Engine};
use ve_scan::VolumeInfo;

const IDENTITY: &str = "uuid:apps-fixture";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start(data: &Path, mount: &Path) -> Engine {
    let volume = VolumeInfo {
        mount: mount.to_path_buf(),
        identity: IDENTITY.into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    Engine::start(
        Config {
            index_path: Some(data.join("index.bin")),
            mounted: vec![volume.clone()],
            volumes: vec![volume],
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    )
}

fn synced_times(engine: &Engine) -> Vec<Option<i64>> {
    engine.shared.index.read().volumes().map(|(_, v)| v.last_synced).collect()
}

fn game(fixture: &Path, appid: &str, title: &str, bytes: usize) {
    let steamapps = fixture.join("library/steamapps");
    write(
        &steamapps.join(format!("appmanifest_{appid}.acf")),
        &format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{title}\"\n\t\"installdir\"\t\t\"{title}\"\n}}\n"),
    );
    write(&steamapps.join(format!("common/{title}/data.bin")), &"x".repeat(bytes));
}

fn by_name<'a>(apps: &'a [App], name: &str) -> &'a App {
    apps.iter()
        .find(|a| a.program.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

#[test]
fn apps_list_sizes_programs_from_their_owner_roots_and_refreshes_after_detection() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let fixture = fixture.path();
    let home = fixture.join("home");
    write(
        &home.join(".local/share/Steam/steamapps/libraryfolders.vdf"),
        &format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
            fixture.join("library").display()
        ),
    );
    game(fixture, "427520", "Factorio", 20_000);
    // A program under the sources' `/opt` whose folder is outside the indexed Location.
    let elsewhere = tempfile::tempdir().unwrap();
    fs::create_dir_all(elsewhere.path().join("opt/tool")).unwrap();

    let engine = start(data.path(), fixture);
    engine.apps.set_sources(Some(Sources {
        root: elsewhere.path().to_path_buf(),
        home: Some(home),
    }));
    wait_for("Last Synced", || synced_times(&engine).iter().any(Option::is_some));

    let apps = engine.apps_list();
    let factorio = by_name(&apps, "Factorio");
    let root = fixture.join("library/steamapps/common/Factorio");
    // Factorio's folder holds one file: its Folder Size is that file's On-disk Size.
    let folder_size = {
        let index = engine.shared.index.read();
        index.on_disk(index.find_path(&root.join("data.bin")).unwrap())
    };
    assert!(folder_size >= 20_000);
    assert_eq!(factorio.size, Some(folder_size));
    assert_eq!(factorio.roots, [(root, folder_size)]);
    let tool = by_name(&apps, "tool");
    assert_eq!((tool.size, tool.roots.len()), (None, 0), "an unindexed program has no size");

    // A game installed since is listed, sized, once the next Sync's Owner detection found it.
    game(fixture, "70", "Half-Life", 5_000);
    engine.sync_location(IDENTITY.to_owned());
    wait_for("Half-Life in apps.list", || {
        engine
            .apps_list()
            .iter()
            .any(|a| a.program.name == "Half-Life" && a.size.is_some_and(|s| s >= 5_000))
    });
    engine.shutdown();
}
