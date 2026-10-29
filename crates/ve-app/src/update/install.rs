//! Self-update (ADR-0007): download the asset for this install, verify its minisign signature, stage it
//! beside what it replaces, and, on "Restart to update", verify the staged asset again, swap it in and
//! relaunch. Nothing is replaced unless the signature checks out on the bytes being installed, and nothing here asks for admin rights except the msi's `msiexec`.

use super::{Release, verify};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const APPIMAGE: &str = "view-everything-x86_64.AppImage";
const MSI: &str = "view-everything-windows-x86_64.msi";
const PORTABLE: &str = "view-everything-windows-x86_64-portable.zip";
const DMG: &str = "view-everything-macos-universal.dmg";
const MAX_ASSET: u64 = 512 * 1024 * 1024;
/// Staged next to the thing it replaces (a rename is only atomic within one directory).
const STAGED: &str = ".view-everything-update";

/// How this copy of the app is installed, and so how it can update itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The AppImage file (`$APPIMAGE`).
    AppImage(PathBuf),
    /// The running exe of an msi install (under Program Files).
    Msi(PathBuf),
    /// The running exe of a portable install.
    Portable(PathBuf),
    /// The `.app` bundle.
    MacApp(PathBuf),
}

/// A verified update waiting for "Restart to update".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    kind: Kind,
    /// What the swap installs: the signed asset itself (AppImage, msi), or what is made from it at the
    /// swap (the exe of the portable zip, the `.app` of the dmg).
    path: PathBuf,
    /// The signed asset as downloaded, read and verified again at the swap: it may have waited on disk
    /// for days, or in a folder other processes can write.
    asset: PathBuf,
    public_key: String,
    signature: String,
}

impl Kind {
    /// The install this process runs from, when it can update itself: `None` (a release link instead)
    /// for other installs and for locations that are not writable.
    pub fn detect() -> Option<Kind> {
        if cfg!(target_os = "linux") {
            appimage_kind(std::env::var_os("APPIMAGE").as_deref())
        } else if cfg!(windows) {
            let exe = std::env::current_exe().ok()?;
            let program_files: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432"]
                .iter()
                .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
                .collect();
            windows_kind(&exe, &program_files)
        } else if cfg!(target_os = "macos") {
            mac_kind(&std::env::current_exe().ok()?)
        } else {
            None
        }
    }

    fn asset(&self) -> &'static str {
        match self {
            Kind::AppImage(_) => APPIMAGE,
            Kind::Msi(_) => MSI,
            Kind::Portable(_) => PORTABLE,
            Kind::MacApp(_) => DMG,
        }
    }

    /// What the update replaces.
    fn target(&self) -> &Path {
        match self {
            Kind::AppImage(p) | Kind::Msi(p) | Kind::Portable(p) | Kind::MacApp(p) => p,
        }
    }

    fn dir(&self) -> &Path {
        self.target().parent().unwrap_or(Path::new("."))
    }

    /// Where the verified update is staged.
    fn staged_path(&self) -> PathBuf {
        match self {
            Kind::AppImage(_) | Kind::Portable(_) => self.dir().join(STAGED),
            Kind::MacApp(_) => self.dir().join(format!("{STAGED}.app")),
            Kind::Msi(_) => unreachable!("an msi is staged in the temporary directory"),
        }
    }
}

fn appimage_kind(appimage: Option<&OsStr>) -> Option<Kind> {
    let path = PathBuf::from(appimage?);
    (path.is_file() && writable(path.parent()?)).then_some(Kind::AppImage(path))
}

fn windows_kind(exe: &Path, program_files: &[PathBuf]) -> Option<Kind> {
    if program_files.iter().any(|p| exe.starts_with(p)) {
        return Some(Kind::Msi(exe.to_owned()));
    }
    writable(exe.parent()?).then(|| Kind::Portable(exe.to_owned()))
}

fn mac_kind(exe: &Path) -> Option<Kind> {
    // .../View Everything.app/Contents/MacOS/view-everything
    let bundle = exe.ancestors().nth(3)?;
    (bundle.extension() == Some(OsStr::new("app")) && writable(bundle.parent()?)).then(|| Kind::MacApp(bundle.to_owned()))
}

/// Whether files can be created in `dir` (a read-only mount, or a directory owned by another user: no).
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!("{STAGED}.probe{}", std::process::id()));
    let ok = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(probe);
    ok
}

fn get(url: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(120)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("User-Agent", "view-everything")
        .call()
        .map_err(|e| e.to_string())?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_ASSET)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))
}

/// Writes `bytes` to `dest` through a `.part` file: `dest` appears whole or not at all.
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let part = dest.with_extension("part");
    let result = fs::write(&part, bytes).and_then(|()| fs::rename(&part, dest));
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result.map_err(|e| format!("could not write {}: {e}", dest.display()))
}

/// Downloads the asset for `kind` from `release`, verifies it against `public_key` and stages it.
/// A bad signature or a failed download leaves nothing behind. `tmp` is where an msi or dmg is kept.
pub fn prepare(kind: &Kind, release: &Release, public_key: &str, tmp: &Path) -> Result<Staged, String> {
    let name = kind.asset();
    let find = |n: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == n)
            .map(|a| a.url.clone())
            .ok_or_else(|| format!("the release has no {n}"))
    };
    let (file_url, sig_url) = (find(name)?, find(&format!("{name}.minisig"))?);
    let signature = String::from_utf8(get(&sig_url)?).map_err(|_| "unreadable signature".to_owned())?;
    let file = get(&file_url)?;
    verify(public_key, &file, &signature)?;

    let (asset, path) = match kind {
        Kind::AppImage(_) => (kind.staged_path(), kind.staged_path()),
        Kind::Msi(_) => {
            let path = tmp.join(format!("{STAGED}.msi"));
            (path.clone(), path)
        }
        Kind::Portable(_) => (kind.dir().join(format!("{STAGED}.zip")), kind.staged_path()),
        Kind::MacApp(_) => (tmp.join(format!("{STAGED}.dmg")), kind.staged_path()),
    };
    write_atomic(&asset, &file)?;
    if matches!(kind, Kind::AppImage(_)) {
        make_executable(&asset)?;
    }
    Ok(Staged {
        kind: kind.clone(),
        path,
        asset,
        public_key: public_key.to_owned(),
        signature,
    })
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

/// The exe inside the portable zip. Nothing is extracted by entry name: only the one root-level
/// entry named exactly `view-everything.exe` is read into memory, and a symlink there is refused.
fn exe_from_zip(zip: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| format!("bad zip: {e}"))?;
    let mut exe = archive
        .by_name("view-everything.exe")
        .map_err(|_| "the zip has no view-everything.exe".to_owned())?;
    if !exe.is_file() || exe.is_symlink() {
        return Err("view-everything.exe in the zip is not a regular file".to_owned());
    }
    let mut bytes = Vec::new();
    exe.by_ref()
        .take(MAX_ASSET)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("bad zip: {e}"))?;
    Ok(bytes)
}

/// Copies the `.app` out of a dmg to `dest`. Only runs on macOS (`hdiutil`, `ditto`).
fn bundle_from_dmg(dmg: &Path, dest: &Path) -> Result<(), String> {
    let mount = dmg.with_extension("mount");
    fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    let run = |program: &str, args: &[&OsStr]| -> Result<(), String> {
        let out = Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
        out.status
            .success()
            .then_some(())
            .ok_or_else(|| format!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim()))
    };
    let attach = ["attach", "-nobrowse", "-readonly", "-noverify", "-mountpoint"].map(OsStr::new);
    run("hdiutil", &[&attach[..], &[mount.as_os_str(), dmg.as_os_str()]].concat())?;
    let copied = (|| {
        let app = fs::read_dir(&mount)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension() == Some(OsStr::new("app")))
            .ok_or("the dmg has no app")?;
        let _ = fs::remove_dir_all(dest);
        run("ditto", &[app.as_os_str(), dest.as_os_str()])
    })();
    let _ = run("hdiutil", &[OsStr::new("detach"), mount.as_os_str(), OsStr::new("-force")]);
    let _ = fs::remove_dir(&mount);
    if copied.is_err() {
        let _ = fs::remove_dir_all(dest);
    }
    copied
}

/// Replaces the current install with the staged update and starts it. The caller then quits.
/// `save` runs once the swap is certain, before the new copy starts (the Index is saved there).
pub fn apply(staged: &Staged, save: impl FnOnce()) -> Result<(), String> {
    install_verified(staged)?;
    if let Kind::Msi(exe) = &staged.kind {
        save();
        return run_msi(&staged.path, exe);
    }
    swap(staged)?;
    save();
    relaunch(&staged.kind)
}

/// Verifies the staged asset and readies what the swap installs from those same bytes: the portable
/// exe is cut out of the verified zip and the `.app` copied out of the verified dmg, so nothing
/// unverified is installed. (`hdiutil` reads the dmg from disk: it is written afresh from the
/// verified bytes just before.)
fn install_verified(staged: &Staged) -> Result<(), String> {
    let bytes = fs::read(&staged.asset).map_err(|e| format!("could not read the update: {e}"))?;
    verify(&staged.public_key, &bytes, &staged.signature)?;
    match &staged.kind {
        Kind::Portable(_) => write_atomic(&staged.path, &exe_from_zip(&bytes)?),
        Kind::MacApp(_) => {
            write_atomic(&staged.asset, &bytes)?;
            bundle_from_dmg(&staged.asset, &staged.path)
        }
        Kind::AppImage(_) | Kind::Msi(_) => Ok(()),
    }
}

/// The swap itself; the running copy keeps working while its file or bundle is replaced.
fn swap(staged: &Staged) -> Result<(), String> {
    let target = staged.kind.target();
    let err = |e: std::io::Error| format!("could not replace {}: {e}", target.display());
    match &staged.kind {
        // One atomic rename: the file is the old AppImage or the new one, never a mix.
        Kind::AppImage(_) => fs::rename(&staged.path, target).map_err(err),
        // A running exe can be renamed on Windows but not overwritten; the old one is removed on next start.
        Kind::Portable(_) => {
            let old = old_exe(target);
            let _ = fs::remove_file(&old);
            fs::rename(target, &old).map_err(err)?;
            fs::rename(&staged.path, target).map_err(|e| {
                let _ = fs::rename(&old, target);
                err(e)
            })
        }
        Kind::MacApp(_) => {
            let old = target.with_extension("app.old");
            let _ = fs::remove_dir_all(&old);
            fs::rename(target, &old).map_err(err)?;
            fs::rename(&staged.path, target).map_err(|e| {
                let _ = fs::rename(&old, target);
                err(e)
            })?;
            let _ = fs::remove_dir_all(old);
            Ok(())
        }
        Kind::Msi(_) => unreachable!("an msi is installed by msiexec"),
    }
}

fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

fn relaunch(kind: &Kind) -> Result<(), String> {
    let mut command = match kind {
        Kind::MacApp(bundle) => {
            let mut open = Command::new("open");
            open.arg("-n").arg(bundle);
            open
        }
        Kind::AppImage(path) | Kind::Portable(path) | Kind::Msi(path) => {
            let mut run = Command::new(path);
            // The new AppImage sets these for itself.
            run.env_remove("APPIMAGE")
                .env_remove("APPDIR")
                .env_remove("ARGV0")
                .env_remove("OWD");
            run
        }
    };
    command.spawn().map(drop).map_err(|e| format!("could not restart: {e}"))
}

/// Runs `msiexec` on the verified msi with admin rights (one UAC prompt unless already elevated) and
/// starts the exe again when it ends, whether or not the upgrade went through. This process quits meanwhile
/// so the installer finds no running copy. The paths travel in the environment, not in the script.
fn run_msi(msi: &Path, exe: &Path) -> Result<(), String> {
    const SCRIPT: &str = "Start-Process msiexec -ArgumentList '/i',('\"' + $env:VE_MSI + '\"'),'/passive','/norestart' \
         -Verb RunAs -Wait -ErrorAction SilentlyContinue; Start-Process -FilePath $env:VE_EXE";
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", SCRIPT])
        .env("VE_MSI", msi)
        .env("VE_EXE", exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0008); // CREATE_NO_WINDOW | DETACHED_PROCESS
    }
    command.spawn().map(drop).map_err(|e| format!("could not run the installer: {e}"))
}

/// At startup: removes what an earlier update left (the renamed-aside exe, a staged file never applied).
pub fn cleanup() {
    for ext in ["msi", "dmg"] {
        let _ = fs::remove_file(std::env::temp_dir().join(format!("{STAGED}.{ext}")));
    }
    let Some(kind) = Kind::detect() else { return };
    if let Kind::Portable(exe) = &kind {
        let _ = fs::remove_file(old_exe(exe));
    }
    if matches!(kind, Kind::Msi(_)) {
        return;
    }
    for entry in fs::read_dir(kind.dir()).into_iter().flatten().filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().starts_with(STAGED) {
            let path = entry.path();
            let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir_all(&path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::Asset;
    use std::io::{Cursor, Write};
    use std::net::TcpListener;

    fn keys() -> (minisign::PublicKey, minisign::SecretKey) {
        let minisign::KeyPair { pk, sk } = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        (pk, sk)
    }

    fn sign(sk: &minisign::SecretKey, pk: &minisign::PublicKey, file: &[u8]) -> String {
        minisign::sign(Some(pk), sk, Cursor::new(file), None, None).unwrap().to_string()
    }

    /// Serves `files` (path, body) on 127.0.0.1 until the process ends. A path ending `/cut` promises more
    /// bytes than it sends, like a dropped connection.
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 2048];
                let n = stream.read(&mut request).unwrap_or(0);
                let path = String::from_utf8_lossy(&request[..n]).split(' ').nth(1).unwrap_or("").to_owned();
                match files.iter().find(|(p, _)| *p == path) {
                    Some((p, body)) => {
                        let promised = if p.ends_with("/cut") { body.len() + 100 } else { body.len() };
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {promised}\r\nConnection: close\r\n\r\n");
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        base
    }

    /// A mock release holding `asset` with `content`, signed by `sk` (or by another key when `forged`).
    fn release(content: &[u8], asset: &str, forged: bool) -> (Release, String) {
        let (pk, sk) = keys();
        let signing = if forged { keys() } else { (pk.clone(), sk) };
        let signature = sign(&signing.1, &signing.0, content);
        let base = serve(vec![
            (format!("/{asset}"), content.to_vec()),
            (format!("/{asset}.minisig"), signature.into_bytes()),
            (format!("/{asset}.cut/cut"), content.to_vec()),
        ]);
        let asset_of = |name: String, url: String| Asset { name, url };
        let release = Release {
            version: "9.9.9".into(),
            notes_url: "https://example.test".into(),
            assets: vec![
                asset_of(asset.into(), format!("{base}/{asset}")),
                asset_of(format!("{asset}.minisig"), format!("{base}/{asset}.minisig")),
            ],
        };
        (release, pk.to_box().unwrap().to_string())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn verified_appimage_replaces_the_old_one_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "old").unwrap();
        let (release, key) = release(b"new", APPIMAGE, false);
        let kind = Kind::AppImage(app.clone());
        let staged = prepare(&kind, &release, &key, dir.path()).unwrap();
        // Staged beside it; the AppImage itself is untouched until the swap.
        assert_eq!(fs::read(&app).unwrap(), b"old");
        swap(&staged).unwrap();
        assert_eq!(fs::read(&app).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["View.AppImage"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&app).unwrap().permissions().mode() & 0o111, 0o111);
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_swaps_and_starts_the_new_appimage() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "old").unwrap();
        let script = b"#!/bin/sh\ntouch \"$(dirname \"$0\")/ran\"\n";
        let (release, key) = release(script, APPIMAGE, false);
        let staged = prepare(&Kind::AppImage(app.clone()), &release, &key, dir.path()).unwrap();
        let mut saved = false;
        apply(&staged, || saved = true).unwrap();
        assert!(saved);
        for _ in 0..100 {
            if dir.path().join("ran").exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the new AppImage did not start");
    }

    #[test]
    fn a_staged_file_changed_since_verification_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "old").unwrap();
        let (release, key) = release(b"new", APPIMAGE, false);
        let staged = prepare(&Kind::AppImage(app.clone()), &release, &key, dir.path()).unwrap();
        fs::write(&staged.path, "swapped by someone").unwrap();
        assert!(apply(&staged, || panic!("must not save")).is_err());
        assert_eq!(fs::read(&app).unwrap(), b"old");
    }

    #[test]
    fn bad_signature_aborts_and_leaves_the_appimage_alone() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "old").unwrap();
        let (forged, key) = release(b"evil", APPIMAGE, true);
        let err = prepare(&Kind::AppImage(app.clone()), &forged, &key, dir.path()).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        // Tampered content under a good signature fails too.
        let (mut tampered, key) = release(b"new", APPIMAGE, false);
        let other = release(b"newer", APPIMAGE, false).0;
        tampered.assets[0] = other.assets[0].clone();
        assert!(prepare(&Kind::AppImage(app.clone()), &tampered, &key, dir.path()).is_err());
        assert_eq!(fs::read(&app).unwrap(), b"old");
        assert_eq!(names(dir.path()), ["View.AppImage"]);
    }

    #[test]
    fn interrupted_download_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "old").unwrap();
        let (mut release, key) = release(b"new", APPIMAGE, false);
        release.assets[0].url = format!("{}.cut/cut", release.assets[0].url);
        assert!(prepare(&Kind::AppImage(app.clone()), &release, &key, dir.path()).is_err());
        assert_eq!(names(dir.path()), ["View.AppImage"]);
        assert_eq!(fs::read(&app).unwrap(), b"old");
    }

    #[test]
    fn a_release_without_the_asset_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (release, key) = release(b"new", MSI, false);
        let kind = Kind::AppImage(dir.path().join("x.AppImage"));
        assert!(
            prepare(&kind, &release, &key, dir.path())
                .unwrap_err()
                .contains("no view-everything")
        );
    }

    #[test]
    fn non_writable_or_missing_locations_have_no_self_update() {
        assert_eq!(appimage_kind(None), None);
        assert_eq!(appimage_kind(Some(OsStr::new("/nonexistent/dir/View.AppImage"))), None);
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("View.AppImage");
        fs::write(&app, "x").unwrap();
        assert_eq!(appimage_kind(Some(app.as_os_str())), Some(Kind::AppImage(app)));
        assert_eq!(names(dir.path()), ["View.AppImage"]);
        assert_eq!(
            mac_kind(Path::new("/nonexistent/View Everything.app/Contents/MacOS/view-everything")),
            None
        );
        assert_eq!(windows_kind(Path::new("/nonexistent/x/view-everything.exe"), &[]), None);
    }

    #[test]
    fn windows_tells_msi_from_portable() {
        let program_files = [PathBuf::from("/pf")];
        let msi = Path::new("/pf/View Everything/view-everything.exe");
        assert_eq!(windows_kind(msi, &program_files), Some(Kind::Msi(msi.to_owned())));
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        assert_eq!(windows_kind(&exe, &program_files), Some(Kind::Portable(exe)));
    }

    fn zip_with_exe(content: &[u8]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        zip.start_file("view-everything.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(content).unwrap();
        zip.finish().unwrap();
        out.into_inner()
    }

    fn zip_with(name: &str, content: &[u8], symlink: bool) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        if symlink {
            zip.add_symlink(name, std::str::from_utf8(content).unwrap(), opts).unwrap();
        } else {
            zip.start_file(name, opts).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn zip_exe_with_dotdot_or_absolute_name_is_not_found() {
        for name in [
            "../view-everything.exe",
            "sub/../../view-everything.exe",
            "/view-everything.exe",
            "C:/view-everything.exe",
        ] {
            let err = exe_from_zip(&zip_with(name, b"evil", false)).unwrap_err();
            assert!(err.contains("no view-everything.exe"), "{name}: {err}");
        }
    }

    #[test]
    fn zip_exe_that_is_a_symlink_is_refused() {
        let err = exe_from_zip(&zip_with("view-everything.exe", b"/etc/passwd", true)).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert_eq!(exe_from_zip(&zip_with_exe(b"ok")).unwrap(), b"ok");
    }

    #[test]
    fn portable_swap_keeps_the_old_exe_aside_until_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        fs::write(&exe, "old").unwrap();
        let (release, key) = release(&zip_with_exe(b"new"), PORTABLE, false);
        let staged = prepare(&Kind::Portable(exe.clone()), &release, &key, dir.path()).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        install_verified(&staged).unwrap();
        swap(&staged).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(old_exe(&exe)).unwrap(), b"old");
        // A second update replaces the leftover of the first.
        fs::write(&staged.path, "newer").unwrap();
        swap(&staged).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"newer");
        assert_eq!(fs::read(old_exe(&exe)).unwrap(), b"new");
    }

    #[test]
    fn portable_zip_changed_since_verification_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        fs::write(&exe, "old").unwrap();
        let (release, key) = release(&zip_with_exe(b"new"), PORTABLE, false);
        let staged = prepare(&Kind::Portable(exe.clone()), &release, &key, dir.path()).unwrap();
        fs::write(&staged.asset, zip_with_exe(b"evil")).unwrap();
        assert!(apply(&staged, || panic!("must not save")).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(!old_exe(&exe).exists());
    }

    #[test]
    fn portable_installs_the_exe_cut_from_the_verified_zip_not_a_planted_one() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        fs::write(&exe, "old").unwrap();
        let (release, key) = release(&zip_with_exe(b"new"), PORTABLE, false);
        let staged = prepare(&Kind::Portable(exe.clone()), &release, &key, dir.path()).unwrap();
        fs::write(&staged.path, "planted").unwrap();
        install_verified(&staged).unwrap();
        swap(&staged).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
    }

    #[test]
    fn mac_dmg_changed_since_verification_is_not_installed() {
        let (dir, tmp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let bundle = dir.path().join("View Everything.app");
        fs::create_dir_all(&bundle).unwrap();
        let (release, key) = release(b"dmg bytes", DMG, false);
        let staged = prepare(&Kind::MacApp(bundle.clone()), &release, &key, tmp.path()).unwrap();
        assert_eq!(fs::read(&staged.asset).unwrap(), b"dmg bytes");
        assert!(!staged.path.exists(), "the bundle is only made at the swap");
        fs::write(&staged.asset, "evil").unwrap();
        assert!(apply(&staged, || panic!("must not save")).is_err());
        assert_eq!(names(dir.path()), ["View Everything.app"]);
    }

    #[test]
    fn portable_bad_signature_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        fs::write(&exe, "old").unwrap();
        let (release, key) = release(&zip_with_exe(b"evil"), PORTABLE, true);
        assert!(prepare(&Kind::Portable(exe.clone()), &release, &key, dir.path()).is_err());
        assert_eq!(names(dir.path()), ["view-everything.exe"]);
    }

    #[test]
    fn msi_is_verified_and_kept_in_the_temp_dir() {
        let (dir, tmp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let exe = dir.path().join("view-everything.exe");
        let (release, key) = release(b"msi bytes", MSI, false);
        let staged = prepare(&Kind::Msi(exe), &release, &key, tmp.path()).unwrap();
        assert_eq!(fs::read(&staged.path).unwrap(), b"msi bytes");
        assert!(names(dir.path()).is_empty());
        let (forged, key) = release_forged(MSI);
        assert!(prepare(&Kind::Msi(dir.path().join("a.exe")), &forged, &key, tmp.path()).is_err());
        assert_eq!(names(tmp.path()).len(), 1);
    }

    fn release_forged(asset: &str) -> (Release, String) {
        release(b"evil", asset, true)
    }

    #[test]
    fn mac_swap_replaces_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("View Everything.app");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents/v"), "old").unwrap();
        let kind = Kind::MacApp(bundle.clone());
        let staged = Staged {
            path: kind.staged_path(),
            asset: PathBuf::new(),
            public_key: String::new(),
            signature: String::new(),
            kind,
        };
        fs::create_dir_all(staged.path.join("Contents")).unwrap();
        fs::write(staged.path.join("Contents/v"), "new").unwrap();
        swap(&staged).unwrap();
        assert_eq!(fs::read(bundle.join("Contents/v")).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["View Everything.app"]);
    }

    #[test]
    fn failed_swap_restores_the_old_exe() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("view-everything.exe");
        fs::write(&exe, "old").unwrap();
        // The staged file is missing: the second rename fails and the first is undone.
        let staged = Staged {
            kind: Kind::Portable(exe.clone()),
            path: dir.path().join("missing"),
            asset: PathBuf::new(),
            public_key: String::new(),
            signature: String::new(),
        };
        assert!(swap(&staged).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }
}
