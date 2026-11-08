use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Action ids the Linux notification protocol reserves for itself.
const RESERVED_ACTION_IDS: [&str; 2] = ["default", "inline-reply"];

/// A desktop notification.
///
/// Fields are public so [`NotificationHandle::update`](crate::NotificationHandle::update)
/// can change them in place.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Notification {
    /// Identifies the notification in [`Event`](crate::Event)s. Showing a second
    /// notification with the same id replaces the first. Survives app restarts on
    /// macOS and Windows, so it's a good place to put a transfer id.
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    pub actions: Vec<Action>,
    pub progress: Option<Progress>,
    /// Path to a local image, such as a thumbnail of the file being transferred.
    pub image: Option<PathBuf>,
    pub sound: Sound,
    pub urgency: Urgency,
    /// Ask the OS to keep the notification on screen until the user responds.
    /// See [`Capabilities::persistent`].
    pub persistent: bool,
    /// Groups related notifications where the OS supports it (macOS, Windows).
    pub group: Option<String>,
}

impl Notification {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: generate_id(),
            title: title.into(),
            subtitle: None,
            body: None,
            actions: Vec::new(),
            progress: None,
            image: None,
            sound: Sound::Default,
            urgency: Urgency::Normal,
            persistent: false,
            group: None,
        }
    }

    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    #[must_use]
    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    #[must_use]
    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    #[must_use]
    pub fn progress(mut self, progress: Progress) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn image(mut self, path: impl Into<PathBuf>) -> Self {
        self.image = Some(path.into());
        self
    }

    #[must_use]
    pub fn sound(mut self, sound: Sound) -> Self {
        self.sound = sound;
        self
    }

    #[must_use]
    pub fn urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    #[must_use]
    pub fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    #[must_use]
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// The body with progress appended as text, for platforms that can't draw a bar.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn body_with_progress_text(&self) -> String {
        let progress = self.progress.as_ref().map(Progress::text_line);
        match (self.body.as_deref(), progress) {
            (Some(body), Some(line)) => format!("{body}\n{line}"),
            (Some(body), None) => body.to_owned(),
            (None, Some(line)) => line,
            (None, None) => String::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |why: String| Err(Error::InvalidNotification(why));
        if self.id.is_empty() {
            return invalid("id must not be empty".into());
        }
        for (i, action) in self.actions.iter().enumerate() {
            if action.id.is_empty() {
                return invalid("action id must not be empty".into());
            }
            if RESERVED_ACTION_IDS.contains(&action.id.as_str()) {
                return invalid(format!("action id {:?} is reserved", action.id));
            }
            if self.actions[..i].iter().any(|a| a.id == action.id) {
                return invalid(format!("duplicate action id {:?}", action.id));
            }
        }
        Ok(())
    }
}

/// A button on a notification. Pressing it produces [`Event::Action`](crate::Event::Action).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Action {
    pub id: String,
    pub label: String,
    pub kind: ActionKind,
    /// Render as a dangerous choice where supported (macOS, Windows 11).
    pub destructive: bool,
    /// Bring the app to the front when pressed (macOS, Windows). Without this, a press
    /// is handled without stealing focus.
    pub foreground: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ActionKind {
    Button,
    /// Collects text from the user. Where unsupported (see
    /// [`Capabilities::text_input`]) it is a plain button and the event has no input.
    TextInput {
        placeholder: Option<String>,
        submit_label: Option<String>,
    },
}

impl Action {
    pub fn button(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: ActionKind::Button,
            destructive: false,
            foreground: false,
        }
    }

    pub fn text_input(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: ActionKind::TextInput {
                placeholder: None,
                submit_label: None,
            },
            ..Self::button(id, label)
        }
    }

    /// Sets the input hint text. No effect on a button.
    #[must_use]
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        if let ActionKind::TextInput { placeholder, .. } = &mut self.kind {
            *placeholder = Some(text.into());
        }
        self
    }

    /// Sets the label of the send button. No effect on a button.
    #[must_use]
    pub fn submit_label(mut self, text: impl Into<String>) -> Self {
        if let ActionKind::TextInput { submit_label, .. } = &mut self.kind {
            *submit_label = Some(text.into());
        }
        self
    }

    #[must_use]
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    #[must_use]
    pub fn foreground(mut self) -> Self {
        self.foreground = true;
        self
    }
}

/// Progress shown on a notification. See [`ProgressSupport`] for how each OS draws it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Progress {
    /// Completed fraction from `0.0` to `1.0`, or `None` for an indeterminate bar.
    pub value: Option<f64>,
    /// What's happening, such as `"Receiving photo.png"`.
    pub status: Option<String>,
    /// Replaces the percentage, such as `"12 MB/s, 2 min left"`.
    pub label: Option<String>,
}

impl Progress {
    /// Creates determinate progress. `value` is clamped to `0.0..=1.0`.
    pub fn fraction(value: f64) -> Self {
        Self {
            value: Some(clamp_fraction(value)),
            status: None,
            label: None,
        }
    }

    pub fn indeterminate() -> Self {
        Self {
            value: None,
            status: None,
            label: None,
        }
    }

    #[must_use]
    pub fn status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The fraction, safe to use even if the public field was set out of range.
    pub(crate) fn clamped_value(&self) -> Option<f64> {
        self.value.map(clamp_fraction)
    }

    pub(crate) fn percent(&self) -> Option<u8> {
        self.clamped_value().map(|v| (v * 100.0).round() as u8)
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.clamped_value() == Some(1.0)
    }

    /// One line of text for platforms without a native bar, such as
    /// `▓▓▓▓░░░░░░ 40% · Receiving · 12 MB/s`.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn text_line(&self) -> String {
        const WIDTH: usize = 10;
        let mut parts = Vec::with_capacity(3);
        if let Some(percent) = self.percent() {
            let filled = usize::from(percent) * WIDTH / 100;
            parts.push(format!(
                "{}{} {percent}%",
                "▓".repeat(filled),
                "░".repeat(WIDTH - filled)
            ));
        }
        parts.extend(self.status.clone());
        parts.extend(self.label.clone());
        parts.join(" · ")
    }
}

fn clamp_fraction(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Sound {
    #[default]
    Default,
    Silent,
    /// A platform-specific sound: a file in the app bundle on macOS, a
    /// `ms-winsoundevent:` URI on Windows, or an XDG sound name on Linux.
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Urgency {
    /// Goes straight to the notification list without interrupting (macOS, Windows).
    Low,
    #[default]
    Normal,
    /// On macOS this needs the time-sensitive notifications entitlement to have an effect.
    High,
}

/// Something the user did to a notification. Delivered to the handler given to
/// [`NotifierBuilder::on_event`](crate::NotifierBuilder::on_event).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The user clicked the notification itself.
    Activated { id: String },
    /// The user pressed an action button.
    Action {
        id: String,
        action_id: String,
        /// Text typed into a [`ActionKind::TextInput`] action.
        input: Option<String>,
    },
    /// The notification went away without the user choosing anything. Not sent when
    /// the app closes or replaces a notification itself.
    Dismissed { id: String, reason: DismissReason },
}

impl Event {
    pub fn id(&self) -> &str {
        match self {
            Self::Activated { id } | Self::Action { id, .. } | Self::Dismissed { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DismissReason {
    User,
    /// Linux only: the notification timed out. macOS and Windows keep timed-out
    /// notifications in their list, where they remain actionable.
    Expired,
}

/// Whether the app may post notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Permission {
    Granted,
    Denied,
    /// macOS only: the user has not been asked yet.
    NotDetermined,
}

/// What the current OS and notification server can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    pub actions: bool,
    pub text_input: bool,
    pub progress: ProgressSupport,
    pub images: bool,
    pub sound: bool,
    /// The OS can be asked to keep a notification on screen. On macOS this is always
    /// false because the user's alert style decides.
    pub persistent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressSupport {
    /// Drawn as a real progress bar.
    Native,
    /// Drawn as a line of text in the notification body.
    Text,
}

fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("zn-{nanos:x}-{n:x}")
}

/// FNV-1a. Unlike `DefaultHasher` its output is stable across Rust releases, which
/// matters because these values are persisted by the OS.
#[cfg_attr(not(any(windows, target_os = "macos", test)), allow(dead_code))]
pub(crate) fn stable_hash(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique() {
        assert_ne!(Notification::new("a").id, Notification::new("a").id);
    }

    #[test]
    fn validate_rejects_bad_actions() {
        let n = |action| Notification::new("t").action(action);
        assert!(n(Action::button("ok", "OK")).validate().is_ok());
        assert!(n(Action::button("", "OK")).validate().is_err());
        assert!(n(Action::button("default", "OK")).validate().is_err());
        assert!(n(Action::button("inline-reply", "OK")).validate().is_err());
        assert!(
            n(Action::button("a", "A"))
                .action(Action::button("a", "B"))
                .validate()
                .is_err()
        );
        assert!(Notification::new("t").id("").validate().is_err());
    }

    #[test]
    fn progress_clamps_out_of_range_values() {
        assert_eq!(Progress::fraction(2.0).percent(), Some(100));
        assert_eq!(Progress::fraction(-1.0).percent(), Some(0));
        assert_eq!(Progress::fraction(f64::NAN).percent(), Some(0));
        let mut p = Progress::fraction(0.5);
        p.value = Some(7.0);
        assert_eq!(p.percent(), Some(100));
        assert_eq!(Progress::indeterminate().percent(), None);
    }

    #[test]
    fn text_line_renders_all_parts() {
        let line = Progress::fraction(0.4)
            .status("Receiving")
            .label("12 MB/s")
            .text_line();
        assert_eq!(line, "▓▓▓▓░░░░░░ 40% · Receiving · 12 MB/s");
        assert_eq!(Progress::fraction(1.0).text_line(), "▓▓▓▓▓▓▓▓▓▓ 100%");
        assert_eq!(
            Progress::indeterminate().status("Waiting").text_line(),
            "Waiting"
        );
    }

    #[test]
    fn stable_hash_is_stable() {
        // These values are persisted by the OS, so they must never change.
        assert_eq!(stable_hash(b""), 0x6c62272e07bb014262b821756295c58d);
        assert_eq!(stable_hash(b"a"), 0xd228cb696f1a8caf78912b704e4a8964);
    }
}
