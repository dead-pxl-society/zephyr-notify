use std::fmt;

/// Errors returned by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// This platform has no notification backend.
    Unsupported,
    /// macOS only: the process isn't running from a `.app` bundle with a bundle
    /// identifier, so `UNUserNotificationCenter` can't be used. This is the normal
    /// result under `cargo run` and `tauri dev`.
    NotBundled,
    /// The user or system policy has not allowed this app to post notifications.
    PermissionDenied,
    /// The notification was rejected before reaching the OS.
    InvalidNotification(String),
    /// The OS notification service reported a failure.
    Backend(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("notifications are not supported on this platform"),
            Self::NotBundled => f.write_str(
                "notifications on macOS require running from a .app bundle with a bundle identifier",
            ),
            Self::PermissionDenied => f.write_str("notification permission was denied"),
            Self::InvalidNotification(why) => write!(f, "invalid notification: {why}"),
            Self::Backend(why) => write!(f, "notification backend error: {why}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
