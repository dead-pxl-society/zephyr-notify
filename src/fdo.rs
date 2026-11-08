//! Maps a [`Notification`] to the freedesktop notification protocol. Free of D-Bus calls
//! so it can be tested on any OS.

use std::collections::HashSet;
use std::path::Path;

use crate::notification::{
    ActionKind, Capabilities, DismissReason, Notification, ProgressSupport, Sound, Urgency,
};
use crate::notifier::Present;

pub(crate) const DEFAULT_ACTION: &str = "default";
pub(crate) const INLINE_REPLY_ACTION: &str = "inline-reply";

/// Servers known to draw the unofficial `value` hint as a bar. It isn't in the spec and
/// servers don't advertise it, so anything else gets a text bar as well, since the hint
/// is ignored where unsupported.
const SERVERS_DRAWING_VALUE_HINT: [&str; 3] = ["dunst", "mako", "SwayNotificationCenter"];

/// What the running notification server supports.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerInfo {
    pub(crate) name: String,
    pub(crate) capabilities: HashSet<String>,
}

impl ServerInfo {
    fn has(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    fn draws_value_hint(&self) -> bool {
        SERVERS_DRAWING_VALUE_HINT
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&self.name))
    }

    pub(crate) fn to_capabilities(&self) -> Capabilities {
        Capabilities {
            actions: self.has("actions"),
            text_input: self.has("inline-reply"),
            progress: if self.draws_value_hint() {
                ProgressSupport::Native
            } else {
                ProgressSupport::Text
            },
            images: self.has("body-images") || self.has("icon-static"),
            sound: self.has("sound"),
            persistent: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hint {
    Str(&'static str, String),
    Byte(&'static str, u8),
    Int(&'static str, i32),
    Bool(&'static str, bool),
}

/// Arguments for the `Notify` D-Bus method, except `replaces_id`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NotifyRequest {
    pub(crate) app_icon: String,
    pub(crate) summary: String,
    pub(crate) body: String,
    /// Alternating action key and label.
    pub(crate) actions: Vec<String>,
    pub(crate) hints: Vec<Hint>,
    /// -1 lets the server decide, 0 never expires.
    pub(crate) expire_timeout: i32,
    /// The action `NotificationReplied` should be reported as.
    pub(crate) reply_action: Option<String>,
}

pub(crate) fn build_request(
    notification: &Notification,
    server: &ServerInfo,
    app_id: &str,
    icon: Option<&Path>,
    present: Present,
) -> NotifyRequest {
    let mut body = if server.draws_value_hint() {
        let status = notification.progress.as_ref().and_then(|progress| {
            match (&progress.status, &progress.label) {
                (Some(status), Some(label)) => Some(format!("{status} · {label}")),
                (Some(text), None) | (None, Some(text)) => Some(text.clone()),
                (None, None) => None,
            }
        });
        [notification.body.clone(), status]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        notification.body_with_progress_text()
    };
    if let Some(subtitle) = &notification.subtitle {
        body = if body.is_empty() {
            subtitle.clone()
        } else {
            format!("{subtitle}\n{body}")
        };
    }
    if server.has("body-markup") {
        body = escape_markup(&body);
    }

    let mut actions = vec![DEFAULT_ACTION.to_owned(), "Open".to_owned()];
    let mut reply_action = None;
    for action in &notification.actions {
        // The protocol allows one inline reply per notification. Everything else is
        // a plain button.
        let is_reply = matches!(action.kind, ActionKind::TextInput { .. })
            && server.has("inline-reply")
            && reply_action.is_none();
        if is_reply {
            reply_action = Some(action.id.clone());
            actions.push(INLINE_REPLY_ACTION.to_owned());
        } else {
            actions.push(action.id.clone());
        }
        actions.push(action.label.clone());
    }

    let mut hints = vec![
        Hint::Str("desktop-entry", app_id.to_owned()),
        Hint::Byte(
            "urgency",
            match notification.urgency {
                Urgency::Low => 0,
                Urgency::Normal => 1,
                Urgency::High => 2,
            },
        ),
    ];
    if let Some(reply) = notification
        .actions
        .iter()
        .find(|a| Some(&a.id) == reply_action.as_ref())
        && let ActionKind::TextInput {
            placeholder,
            submit_label,
        } = &reply.kind
    {
        if let Some(text) = placeholder {
            hints.push(Hint::Str("x-kde-reply-placeholder-text", text.clone()));
        }
        if let Some(text) = submit_label {
            hints.push(Hint::Str("x-kde-reply-submit-button-text", text.clone()));
        }
    }
    if let Some(percent) = notification.progress.as_ref().and_then(|p| p.percent()) {
        hints.push(Hint::Int("value", i32::from(percent)));
    }
    if let Some(uri) = notification.image.as_deref().and_then(path_to_uri) {
        hints.push(Hint::Str("image-path", uri));
    }
    let silent = present == Present::Silent || notification.sound == Sound::Silent;
    if silent {
        hints.push(Hint::Bool("suppress-sound", true));
    } else if let Sound::Named(name) = &notification.sound {
        hints.push(Hint::Str("sound-name", name.clone()));
    }

    NotifyRequest {
        app_icon: icon
            .and_then(path_to_uri)
            .unwrap_or_else(|| app_id.to_owned()),
        summary: notification.title.clone(),
        body,
        actions,
        hints,
        // Keep a live progress bar around, since expiring ends its updates.
        expire_timeout: if notification.persistent || notification.progress.is_some() {
            0
        } else {
            -1
        },
        reply_action,
    }
}

pub(crate) fn dismiss_reason(code: u32) -> Option<DismissReason> {
    match code {
        1 => Some(DismissReason::Expired),
        2 => Some(DismissReason::User),
        // 3 is our own CloseNotification call. Anything else is unspecified, and is what
        // some servers send when an action closes the notification.
        _ => None,
    }
}

/// Peer-supplied names reach this body, and some servers would render them as markup.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn path_to_uri(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    if !path.starts_with('/') {
        return None;
    }
    let mut out = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::{Action, Progress};

    fn server(name: &str, caps: &[&str]) -> ServerInfo {
        ServerInfo {
            name: name.to_owned(),
            capabilities: caps.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn build(n: &Notification, s: &ServerInfo) -> NotifyRequest {
        build_request(n, s, "com.example.app", None, Present::Alert)
    }

    #[test]
    fn actions_alternate_key_and_label_after_default() {
        let n = Notification::new("t")
            .action(Action::button("accept", "Accept"))
            .action(Action::button("reject", "Reject"));
        assert_eq!(
            build(&n, &server("x", &["actions"])).actions,
            ["default", "Open", "accept", "Accept", "reject", "Reject"]
        );
    }

    #[test]
    fn body_is_escaped_only_when_server_renders_markup() {
        let n = Notification::new("t").body("<b>x</b> & y.png");
        assert_eq!(build(&n, &server("x", &[])).body, "<b>x</b> & y.png");
        assert_eq!(
            build(&n, &server("x", &["body-markup"])).body,
            "&lt;b&gt;x&lt;/b&gt; &amp; y.png"
        );
    }

    #[test]
    fn known_servers_get_only_the_hint() {
        let n = Notification::new("t").progress(Progress::fraction(0.5).status("Receiving"));
        let r = build(&n, &server("dunst", &[]));
        assert!(r.hints.contains(&Hint::Int("value", 50)));
        assert_eq!(r.body, "Receiving");
    }

    #[test]
    fn unknown_servers_also_get_a_text_bar() {
        let n = Notification::new("t")
            .body("b")
            .progress(Progress::fraction(0.5));
        let r = build(&n, &server("gnome-shell", &[]));
        assert!(r.hints.contains(&Hint::Int("value", 50)));
        assert_eq!(r.body, "b\n▓▓▓▓▓░░░░░ 50%");
    }

    #[test]
    fn indeterminate_progress_sends_no_value_hint() {
        let n = Notification::new("t").progress(Progress::indeterminate());
        assert!(
            !build(&n, &server("dunst", &[]))
                .hints
                .iter()
                .any(|h| matches!(h, Hint::Int(..)))
        );
    }

    #[test]
    fn inline_reply_needs_server_support() {
        let n =
            Notification::new("t").action(Action::text_input("reply", "Reply").placeholder("Hi"));
        let unsupported = build(&n, &server("x", &["actions"]));
        assert_eq!(unsupported.actions[2], "reply");
        assert_eq!(unsupported.reply_action, None);
        let supported = build(&n, &server("x", &["actions", "inline-reply"]));
        assert_eq!(supported.actions[2], "inline-reply");
        assert_eq!(supported.reply_action.as_deref(), Some("reply"));
        assert!(
            supported
                .hints
                .contains(&Hint::Str("x-kde-reply-placeholder-text", "Hi".into()))
        );
    }

    #[test]
    fn second_text_input_is_a_plain_button() {
        let n = Notification::new("t")
            .action(Action::text_input("a", "A"))
            .action(Action::text_input("b", "B"));
        let r = build(&n, &server("x", &["inline-reply"]));
        assert_eq!(r.actions[2..], ["inline-reply", "A", "b", "B"]);
        assert_eq!(r.reply_action.as_deref(), Some("a"));
    }

    #[test]
    fn expiry() {
        let s = server("x", &[]);
        assert_eq!(build(&Notification::new("t"), &s).expire_timeout, -1);
        assert_eq!(
            build(&Notification::new("t").persistent(true), &s).expire_timeout,
            0
        );
        assert_eq!(
            build(
                &Notification::new("t").progress(Progress::fraction(0.0)),
                &s
            )
            .expire_timeout,
            0
        );
    }

    #[test]
    fn silent_updates_suppress_sound() {
        let n = Notification::new("t").sound(Sound::Named("message".into()));
        let s = server("x", &[]);
        let silent = build_request(&n, &s, "a", None, Present::Silent);
        assert!(silent.hints.contains(&Hint::Bool("suppress-sound", true)));
        assert!(
            !silent
                .hints
                .iter()
                .any(|h| matches!(h, Hint::Str("sound-name", _)))
        );
        assert!(
            build(&n, &s)
                .hints
                .contains(&Hint::Str("sound-name", "message".into()))
        );
    }

    #[test]
    fn subtitle_goes_first_in_body() {
        let n = Notification::new("t").subtitle("s").body("b");
        assert_eq!(build(&n, &server("x", &[])).body, "s\nb");
    }

    #[test]
    fn dismiss_codes() {
        assert_eq!(dismiss_reason(1), Some(DismissReason::Expired));
        assert_eq!(dismiss_reason(2), Some(DismissReason::User));
        assert_eq!(dismiss_reason(3), None);
        assert_eq!(dismiss_reason(4), None);
    }

    #[test]
    fn image_paths_become_encoded_uris() {
        let n = Notification::new("t").image("/tmp/a b/é.png");
        assert!(build(&n, &server("x", &[])).hints.contains(&Hint::Str(
            "image-path",
            "file:///tmp/a%20b/%C3%A9.png".into()
        )));
        let relative = Notification::new("t").image("rel.png");
        assert!(
            !build(&relative, &server("x", &[]))
                .hints
                .iter()
                .any(|h| matches!(h, Hint::Str("image-path", _)))
        );
    }
}
