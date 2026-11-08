use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::notification::{Capabilities, Event, Notification, Permission, Progress};
use crate::platform;

/// How a notification is being put on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Present {
    /// Alert the user: banner, sound.
    Alert,
    /// Change an existing notification as quietly as the OS allows.
    Silent,
}

/// App identity, used differently by each OS.
#[derive(Debug, Clone)]
#[cfg_attr(target_os = "macos", allow(dead_code))] // macOS reads all of this from the bundle.
pub(crate) struct AppConfig {
    pub(crate) app_id: String,
    pub(crate) app_name: String,
    pub(crate) icon: Option<PathBuf>,
}

/// Lets backends report events from whatever thread the OS calls them on.
///
/// The user's handler runs on a dedicated thread, so a slow handler can't stall an OS
/// callback, and a handler can call back into the [`Notifier`] without deadlocking.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: Sender<Event>,
    live: Arc<Mutex<HashSet<String>>>,
}

impl EventSink {
    pub(crate) fn emit(&self, event: Event) {
        // Any event means the notification is gone, so a later progress tick must not
        // bring it back.
        lock(&self.live).remove(event.id());
        // Fails only if the handler thread is gone, so nobody is listening anyway.
        let _ = self.tx.send(event);
    }
}

/// Builds a [`Notifier`].
pub struct NotifierBuilder {
    config: AppConfig,
    handler: Option<Box<dyn FnMut(Event) + Send>>,
}

impl NotifierBuilder {
    /// Display name, used on Linux and Windows. macOS uses the bundle's name.
    #[must_use]
    pub fn app_name(mut self, name: impl Into<String>) -> Self {
        self.config.app_name = name.into();
        self
    }

    /// Icon path, used on Linux and Windows. macOS uses the bundle's icon.
    #[must_use]
    pub fn icon(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.icon = Some(path.into());
        self
    }

    /// Sets the function called for each [`Event`], on a dedicated thread.
    ///
    /// Set this before [`build`](Self::build) so that a button press which launched the
    /// app isn't lost.
    #[must_use]
    pub fn on_event(mut self, handler: impl FnMut(Event) + Send + 'static) -> Self {
        self.handler = Some(Box::new(handler));
        self
    }

    /// Connects to the OS notification service.
    ///
    /// Call this early in startup and only once per process. macOS and Windows deliver
    /// the button press that launched the app as soon as this runs.
    pub fn build(self) -> Result<Notifier> {
        let (tx, rx) = mpsc::channel::<Event>();
        let mut handler = self.handler;
        thread::Builder::new()
            .name("zephyr-notify-events".into())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    if let Some(handler) = handler.as_mut() {
                        // One bad event must not stop all later events.
                        if catch_unwind(AssertUnwindSafe(|| handler(event))).is_err() {
                            log::error!("notification event handler panicked");
                        }
                    }
                }
            })
            .map_err(|e| crate::Error::Backend(format!("failed to spawn event thread: {e}")))?;

        let live = Arc::new(Mutex::new(HashSet::new()));
        let sink = EventSink {
            tx,
            live: live.clone(),
        };
        let backend = platform::PlatformBackend::new(&self.config, sink)?;
        Ok(Notifier {
            inner: Arc::new(Inner { backend, live }),
        })
    }
}

struct Inner {
    backend: platform::PlatformBackend,
    /// Notifications on screen, as far as we know.
    live: Arc<Mutex<HashSet<String>>>,
}

/// Entry point for showing notifications. Cheap to clone.
///
/// Methods make a short round trip to the OS, so they're fine to call from async code,
/// except [`request_permission`](Self::request_permission).
#[derive(Clone)]
pub struct Notifier {
    inner: Arc<Inner>,
}

impl Notifier {
    /// Starts building a notifier.
    ///
    /// `app_id` should be your Tauri `identifier`. It's the AppUserModelID on Windows
    /// and the desktop entry name on Linux. macOS uses the bundle identifier.
    pub fn builder(app_id: impl Into<String>) -> NotifierBuilder {
        let app_id = app_id.into();
        NotifierBuilder {
            config: AppConfig {
                app_name: app_id.clone(),
                app_id,
                icon: None,
            },
            handler: None,
        }
    }

    /// Shows a notification, replacing any existing one with the same id.
    pub fn show(&self, notification: Notification) -> Result<NotificationHandle> {
        notification.validate()?;
        self.inner.backend.show(&notification, Present::Alert)?;
        lock(&self.inner.live).insert(notification.id.clone());
        Ok(NotificationHandle {
            inner: self.inner.clone(),
            state: Arc::new(Mutex::new(HandleState {
                notification,
                last_progress: None,
            })),
        })
    }

    /// Closes a notification by id, including one left over from an earlier run.
    pub fn close(&self, id: &str) -> Result<()> {
        lock(&self.inner.live).remove(id);
        self.inner.backend.close(id)
    }

    /// Closes every notification from this app.
    pub fn close_all(&self) -> Result<()> {
        lock(&self.inner.live).clear();
        self.inner.backend.close_all()
    }

    pub fn capabilities(&self) -> Capabilities {
        self.inner.backend.capabilities()
    }

    pub fn permission(&self) -> Result<Permission> {
        self.inner.backend.permission()
    }

    /// Asks the user for permission if they haven't been asked yet.
    ///
    /// On macOS this **blocks until the user answers**. Don't call it on the main
    /// thread or an async task; use `tauri::async_runtime::spawn_blocking`.
    pub fn request_permission(&self) -> Result<Permission> {
        self.inner.backend.request_permission()
    }
}

struct HandleState {
    notification: Notification,
    last_progress: Option<Instant>,
}

/// A live notification. Cheap to clone and can be shared across threads.
///
/// Dropping the handle leaves the notification on screen.
///
/// Updates do nothing once the notification is gone, so a dismissed notification doesn't
/// reappear on the next progress tick. Use [`Notifier::show`] to alert the user again.
#[derive(Clone)]
pub struct NotificationHandle {
    inner: Arc<Inner>,
    state: Arc<Mutex<HandleState>>,
}

impl NotificationHandle {
    pub fn id(&self) -> String {
        lock(&self.state).notification.id.clone()
    }

    /// Whether the notification is still on screen, as far as this process knows.
    pub fn is_active(&self) -> bool {
        // Taken one at a time: `set_progress` holds `state` while it locks `live`.
        let id = self.id();
        lock(&self.inner.live).contains(&id)
    }

    /// Updates the progress. Safe to call in a tight loop: calls are rate limited to
    /// what the OS handles well, but reaching 100% is always sent.
    pub fn set_progress(&self, progress: Progress) -> Result<()> {
        let interval = self.inner.backend.min_progress_interval();
        let mut state = lock(&self.state);
        let throttled = !progress.is_complete()
            && state
                .last_progress
                .is_some_and(|last| last.elapsed() < interval);
        state.notification.progress = Some(progress);
        if throttled {
            // Kept in `state`, so the next update that goes through includes it.
            return Ok(());
        }
        let result = self.push(&state.notification);
        state.last_progress = Some(Instant::now());
        result
    }

    /// Edits the notification and quietly applies the change. The id can't be changed.
    pub fn update(&self, edit: impl FnOnce(&mut Notification)) -> Result<()> {
        let mut state = lock(&self.state);
        let mut edited = state.notification.clone();
        edit(&mut edited);
        edited.id.clone_from(&state.notification.id);
        edited.validate()?;
        state.notification = edited;
        self.push(&state.notification)
    }

    /// Replaces this notification and alerts the user again, even if they dismissed it.
    /// Use it to go from "Receiving…" to "Transfer complete".
    pub fn alert(&self, edit: impl FnOnce(&mut Notification)) -> Result<()> {
        let mut state = lock(&self.state);
        let mut edited = state.notification.clone();
        edit(&mut edited);
        edited.id.clone_from(&state.notification.id);
        edited.validate()?;
        self.inner.backend.show(&edited, Present::Alert)?;
        lock(&self.inner.live).insert(edited.id.clone());
        state.notification = edited;
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        let id = self.id();
        lock(&self.inner.live).remove(&id);
        self.inner.backend.close(&id)
    }

    fn push(&self, notification: &Notification) -> Result<()> {
        if !lock(&self.inner.live).contains(&notification.id) {
            return Ok(());
        }
        self.inner.backend.show(notification, Present::Silent)
    }
}

/// Locks a mutex, ignoring poisoning: all guarded data stays valid if a holder panics.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Implemented by each OS backend.
pub(crate) trait BackendApi: Sized + Send + Sync {
    fn new(config: &AppConfig, sink: EventSink) -> Result<Self>;
    /// Shows the notification, or replaces one with the same id.
    fn show(&self, notification: &Notification, present: Present) -> Result<()>;
    fn close(&self, id: &str) -> Result<()>;
    fn close_all(&self) -> Result<()>;
    fn capabilities(&self) -> Capabilities;
    fn permission(&self) -> Result<Permission>;
    fn request_permission(&self) -> Result<Permission>;
    /// The fastest rate progress updates should be sent at.
    fn min_progress_interval(&self) -> Duration;
}
