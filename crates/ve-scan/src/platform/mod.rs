//! Operating-system specific Volume discovery and file facts.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::discover_volumes;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::discover_volumes;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::discover_volumes;
