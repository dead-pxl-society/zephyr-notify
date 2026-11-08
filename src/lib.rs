//! Native desktop notifications with action buttons and progress, behind one API for
//! macOS, Windows and Linux.
//!
//! ```no_run
//! use zephyr_notify::{Action, Event, Notification, Notifier, Progress};
//!
//! # fn main() -> zephyr_notify::Result<()> {
//! let notifier = Notifier::builder("com.example.zephyr")
//!     .app_name("Zephyr")
//!     .on_event(|event| match event {
//!         Event::Action { id, action_id, .. } => println!("{id}: pressed {action_id}"),
//!         other => println!("{other:?}"),
//!     })
//!     .build()?;
//! notifier.request_permission()?;
//!
//! let request = notifier.show(
//!     Notification::new("Alex wants to send you a file")
//!         .id("transfer-42")
//!         .body("holiday.mov (1.2 GB)")
//!         .action(Action::button("accept", "Accept"))
//!         .action(Action::button("reject", "Reject").destructive())
//!         .persistent(true),
//! )?;
//!
//! // Later, once the user has accepted:
//! request.alert(|n| {
//!     n.title = "Receiving holiday.mov".into();
//!     n.actions.clear();
//!     n.progress = Some(Progress::fraction(0.0));
//! })?;
//! request.set_progress(Progress::fraction(0.25).label("12 MB/s"))?;
//! # Ok(())
//! # }
//! ```
//!
//! See the README for each platform's setup requirements and differences.

mod error;
mod notification;
mod notifier;
mod platform;

#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
mod toast_xml;

#[cfg(any(
    test,
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
#[cfg_attr(test, allow(dead_code))]
mod fdo;

pub use error::{Error, Result};
pub use notification::{
    Action, ActionKind, Capabilities, DismissReason, Event, Notification, Permission, Progress,
    ProgressSupport, Sound, Urgency,
};
pub use notifier::{NotificationHandle, Notifier, NotifierBuilder};

#[cfg(windows)]
pub use platform::unregister_windows_app;
