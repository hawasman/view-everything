//! The saved shapes of Remote (settings) and SyncState (Index) from v0.1.0 must keep loading.
use std::path::Path;
use ve_core::{AzureRemote, EntryMeta, FtpRemote, FtpTls, Index, Remote, S3Remote, SyncState, WebdavRemote};

#[derive(serde::Deserialize)]
struct Settings {
    remotes: Vec<Remote>,
}

#[test]
fn a_v0_1_0_settings_file_loads_ftp_ssh_and_s3_remotes() {
    let text = r#"
[[remotes]]
host = "ftp.example.com"
port = 2121
user = "me"
path = "/pub"
tls = "plain"

[[remotes]]
host = "me@box"
path = "/srv"

[[remotes]]
bucket = "bkt"
prefix = "a/b"
endpoint = "https://minio.lan:9000"
region = "us-east-1"
path_style = true
access_key = "AKIAEXAMPLE"
"#;
    let s: Settings = toml::from_str(text).unwrap();
    assert_eq!(s.remotes.len(), 3);
    assert!(matches!(&s.remotes[0], Remote::Ftp(f) if f.port == 2121 && f.tls == FtpTls::Plain));
    assert_eq!(s.remotes[1], Remote::new("me@box", "/srv").unwrap());
    assert!(matches!(&s.remotes[2], Remote::S3(r) if r.access_key.as_deref() == Some("AKIAEXAMPLE")));
}

#[test]
fn a_saved_index_keeps_every_sync_state_and_their_variant_order() {
    let states = [
        SyncState::Walk,
        SyncState::Ssh(match Remote::new("me@box", "/srv").unwrap() {
            Remote::Ssh(r) => r,
            _ => unreachable!(),
        }),
        SyncState::Ntfs {
            journal_id: 7,
            next_usn: 9,
        },
        SyncState::S3(S3Remote::new("b", "p", "minio.lan:9000", "us-east-1", false, "work", "AKIA").unwrap()),
        SyncState::Ftp(FtpRemote::new("h", 21, "u", "/", FtpTls::Explicit).unwrap()),
        SyncState::Webdav(WebdavRemote::new("https://h/dav", "u").unwrap()),
        // `endpoint: None` too: unlike S3's skipped fields it must round-trip through bincode.
        SyncState::Azure(AzureRemote::new("acct", "c", "p", "", true).unwrap()),
    ];
    // bincode writes the variant index first: this order is the v0.1.0 wire format.
    for (i, s) in states.iter().enumerate() {
        assert_eq!(bincode::serialize(s).unwrap()[..4], (i as u32).to_le_bytes());
    }
    let mut index = Index::new();
    for (i, s) in states.iter().enumerate() {
        let meta = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        index.add_volume(Path::new(&format!("/v{i}")), "x", &format!("v{i}"), meta, s.clone());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.save(&path).unwrap();
    let back = Index::load(&path).unwrap();
    let loaded: Vec<_> = back.volumes().map(|(_, v)| v.sync.clone()).collect();
    assert_eq!(loaded, states);
}

#[test]
fn an_s3_remote_with_unset_options_survives_a_saved_index() {
    // A skipped `None` would shift every field after it: the saved Index could not be read.
    let remote = S3Remote::new("b", "", "", "us-east-1", false, "", "").unwrap();
    assert!(remote.endpoint.is_none() && remote.profile.is_none() && remote.access_key.is_none());
    let mut index = Index::new();
    let meta = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    index.add_volume(Path::new("/s3"), "s3", "s3", meta, SyncState::S3(remote.clone()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.save(&path).unwrap();
    let back = Index::load(&path).unwrap();
    assert_eq!(back.volumes().next().unwrap().1.sync, SyncState::S3(remote));
}

#[test]
fn webdav_and_azure_remotes_round_trip_settings_and_are_told_apart() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Settings {
        remotes: Vec<Remote>,
    }
    let remotes = vec![
        Remote::Webdav(WebdavRemote::new("https://h:8443/dav/", "me").unwrap()),
        Remote::Azure(AzureRemote::new("acct", "c", "p/q", "", false).unwrap()),
        Remote::Azure(AzureRemote::new("devstoreaccount1", "c", "", "http://127.0.0.1:10000/devstoreaccount1", true).unwrap()),
        Remote::new("me@box", "/srv").unwrap(),
    ];
    let text = toml::to_string(&Settings { remotes: remotes.clone() }).unwrap();
    let back: Settings = toml::from_str(&text).unwrap();
    assert_eq!(back.remotes, remotes);
}

#[test]
fn a_remote_name_is_display_only_and_changes_neither_identity_nor_the_saved_index() {
    let remote = Remote::new("me@box", "/srv").unwrap();
    assert_eq!(remote.display_name(None), "me@box:/srv");
    assert_eq!(remote.display_name(Some("  ")), "me@box:/srv");
    assert_eq!(remote.display_name(Some(" Backup box ")), "Backup box");
    let mut index = Index::new();
    let id = index.add_remote(&remote);
    let root = index.volume(id).unwrap().root;
    assert_eq!(index.display_name(root), "ssh:me@box:/srv");
    index.set_remote_name(id, Some(" Backup box "));
    assert_eq!(index.display_name(root), "Backup box");
    assert_eq!(index.name(root), "ssh:me@box:/srv");
    assert_eq!(index.volume(id).unwrap().remote_display_name().as_deref(), Some("Backup box"));
    // The name is not saved in the Index: the v0.1.0 layout is unchanged and loads as before.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.save(&path).unwrap();
    let back = Index::load(&path).unwrap();
    let volume = back.volumes().next().unwrap().1;
    assert_eq!(volume.remote(), Some(remote));
    assert_eq!(volume.name, None);
}

#[test]
fn webdav_and_azure_remotes_are_validated_and_identified() {
    let dav = WebdavRemote::new(" https://h/dav// ", " me ").unwrap();
    assert_eq!(
        (dav.url.as_str(), dav.identity().as_str()),
        ("https://h/dav", "webdav:me@https://h/dav")
    );
    assert_eq!(WebdavRemote::new("http://h", "").unwrap().identity(), "webdav:http://h");
    for bad in [
        "h/dav",
        "ftp://h",
        "https://",
        "https://me@h/dav",
        "https://h/a b",
        "https://h/?x=1",
    ] {
        assert!(WebdavRemote::new(bad, "").is_err(), "{bad}");
    }
    assert!(WebdavRemote::new("https://h", "a:b").is_err());
    let az = AzureRemote::new("acct", "c", "/p/", "", false).unwrap();
    assert_eq!(az.identity(), "azure:acct.blob.core.windows.net/c/p");
    let azurite = AzureRemote::new("devstoreaccount1", "c", "", "127.0.0.1:10000/devstoreaccount1/", false).unwrap();
    assert_eq!(azurite.identity(), "azure:127.0.0.1:10000/devstoreaccount1/c/");
    assert!(AzureRemote::new("a_b", "c", "", "", false).is_err());
    assert!(AzureRemote::new("a", "c/d", "", "", false).is_err());
}
