//! Selects the OS backend. Every backend implements [`BackendApi`](crate::notifier::BackendApi).

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::MacBackend as PlatformBackend;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use self::windows::WindowsBackend as PlatformBackend;
#[cfg(windows)]
pub use self::windows::unregister_windows_app;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod linux;
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
pub(crate) use linux::LinuxBackend as PlatformBackend;

#[cfg(not(any(
    target_os = "macos",
    windows,
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
mod unsupported;
#[cfg(not(any(
    target_os = "macos",
    windows,
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
pub(crate) use unsupported::UnsupportedBackend as PlatformBackend;
