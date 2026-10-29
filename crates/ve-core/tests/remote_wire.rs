//! The saved shapes of Remote (settings) and SyncState (Index) from v0.1.0 must keep loading.
use std::path::Path;
use ve_core::{EntryMeta, FtpRemote, FtpTls, Index, Remote, S3Remote, SyncState};

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
        SyncState::S3(
            // All optional fields set: bincode cannot read back a skipped `None` (a known gap, see the report).
            S3Remote::new("b", "p", "minio.lan:9000", "us-east-1", false, "work", "AKIA").unwrap(),
        ),
        SyncState::Ftp(FtpRemote::new("h", 21, "u", "/", FtpTls::Explicit).unwrap()),
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
