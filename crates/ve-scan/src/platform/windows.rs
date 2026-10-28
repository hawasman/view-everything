//! Windows Volume discovery, elevation and file facts.

use crate::volumes::{VolumeInfo, share_identity};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetCompressedFileSizeW, GetDiskFreeSpaceW, GetDriveTypeW, GetLogicalDriveStringsW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_FIXED: u32 = 3;
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
const INVALID_FILE_SIZE: u32 = u32::MAX;

fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
        .to_string_lossy()
        .into_owned()
}

/// Fixed and removable local drives, and mapped network drives.
pub fn discover_volumes() -> Vec<VolumeInfo> {
    let mut buf = vec![0u16; 512];
    // SAFETY: the buffer length passed matches the allocation.
    let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    if n == 0 || n > buf.len() {
        return Vec::new();
    }
    let mut volumes = Vec::new();
    for root in buf[..n].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let root_z: Vec<u16> = root.iter().copied().chain(std::iter::once(0)).collect();
        // SAFETY: `root_z` is NUL terminated.
        let kind = unsafe { GetDriveTypeW(root_z.as_ptr()) };
        let (removable, network) = match kind {
            DRIVE_FIXED => (false, false),
            DRIVE_REMOVABLE => (true, false),
            DRIVE_REMOTE => (false, true),
            _ => continue,
        };
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        let mut flags = 0u32;
        let mut serial = 0u32;
        // SAFETY: all out-buffers are valid for the lengths given.
        let ok = unsafe {
            GetVolumeInformationW(
                root_z.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                std::ptr::null_mut(),
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // no media, or unreadable
        }
        volumes.push(VolumeInfo {
            identity: match network.then(|| remote_name(root)).flatten() {
                Some(unc) => share_identity(&unc),
                None => identity(&root_z, serial),
            },
            network,
            mount: PathBuf::from(OsString::from_wide(root)),
            fs_type: from_wide(&fs),
            label: from_wide(&label),
            removable,
            read_only: flags & FILE_READ_ONLY_VOLUME != 0,
        });
    }
    volumes
}

/// The `\\server\share` a mapped drive root such as `Z:\` points at.
fn remote_name(root: &[u16]) -> Option<String> {
    let local = [root[0], u16::from(b':'), 0];
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `local` is NUL terminated and `buf` is valid for `len` characters.
    let err = unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) };
    (err == NO_ERROR).then(|| from_wide(&buf))
}

/// The Volume GUID (`\\?\Volume{...}\`), which stays with the volume whatever letter it is
/// given; the volume serial number if Windows has no GUID for it.
fn identity(root_z: &[u16], serial: u32) -> String {
    let mut name = [0u16; 64];
    // SAFETY: `root_z` is NUL terminated and `name` is valid for the length given.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root_z.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    };
    if ok != 0 {
        format!("guid:{}", from_wide(&name))
    } else {
        format!("serial:{serial:08X}")
    }
}

/// Whether this process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` receives a handle we close below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: the output buffer is a TOKEN_ELEVATION of the stated size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` is a valid open handle.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Starts an elevated copy of this program with the same arguments. `Ok(true)` means it
/// started and the caller should exit; `Ok(false)` means the user declined the prompt.
pub fn relaunch_elevated() -> std::io::Result<bool> {
    let exe = wide(std::env::current_exe()?);
    let args: Vec<String> = std::env::args().skip(1).map(|a| quote_arg(&a)).collect();
    let params = wide(args.join(" "));
    let verb = wide("runas");
    // SAFETY: zeroed SHELLEXECUTEINFOW is a valid blank request; fields are set below.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: all string pointers outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        if !info.hProcess.is_null() {
            // SAFETY: handle returned because of SEE_MASK_NOCLOSEPROCESS.
            unsafe { CloseHandle(info.hProcess) };
        }
        return Ok(true);
    }
    // SAFETY: plain thread-local error read.
    let code = unsafe { GetLastError() };
    if code == ERROR_CANCELLED {
        Ok(false)
    } else {
        Err(std::io::Error::from_raw_os_error(code as i32))
    }
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Storage the file occupies on disk: its compressed/sparse-aware size from the filesystem,
/// rounded up to the Volume's cluster size. `None` if it cannot be queried.
pub fn on_disk_size(path: &Path) -> Option<u64> {
    let (extended, drive) = extended_path(path)?;
    let mut high = 0u32;
    // SAFETY: `extended` is NUL terminated; `high` is a valid out pointer.
    let low = unsafe { GetCompressedFileSizeW(extended.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    let size = (u64::from(high) << 32) | u64::from(low);
    Some(match cluster_size(drive) {
        Some(cluster) => size.div_ceil(cluster) * cluster,
        None => size,
    })
}

/// The `\\?\`-prefixed, NUL-terminated form of an absolute drive path (so paths beyond
/// MAX_PATH work), and its drive letter.
fn extended_path(path: &Path) -> Option<(Vec<u16>, char)> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let drive = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            (letter as char).to_ascii_uppercase()
        }
        _ => return None,
    };
    if matches!(prefix.kind(), Prefix::VerbatimDisk(_)) {
        return Some((wide(path), drive));
    }
    let mut text = OsString::from(r"\\?\");
    text.push(path);
    // The verbatim form does not normalise `/`.
    let text: String = text.to_string_lossy().replace('/', "\\");
    Some((wide(text), drive))
}

fn cluster_size(drive: char) -> Option<u64> {
    static CACHE: OnceLock<Mutex<HashMap<char, u64>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&size) = cache.lock().unwrap().get(&drive) {
        return Some(size);
    }
    let root = wide(format!("{drive}:\\"));
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `root` is NUL terminated and the out pointers are valid.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors,
            &mut bytes,
            &mut free,
            &mut total,
        )
    };
    let size = u64::from(sectors) * u64::from(bytes);
    if ok == 0 || size == 0 {
        return None;
    }
    cache.lock().unwrap().insert(drive, size);
    Some(size)
}
