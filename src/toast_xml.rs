//! Builds Windows toast XML and the argument strings that identify a button press.
//! Free of Windows APIs so it can be tested on any OS.

use crate::notification::{ActionKind, Notification, Sound, Urgency, stable_hash};

/// Data binding keys. Bound values can be changed without re-creating the toast.
pub(crate) const BIND_TITLE: &str = "zn_title";
pub(crate) const BIND_BODY: &str = "zn_body";
pub(crate) const BIND_PROGRESS_VALUE: &str = "zn_progress_value";
pub(crate) const BIND_PROGRESS_LABEL: &str = "zn_progress_label";
pub(crate) const BIND_PROGRESS_STATUS: &str = "zn_progress_status";

/// Windows rejects a longer tag or group.
const MAX_TAG_LEN: usize = 64;

/// A button press or body click, decoded from toast arguments.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ToastArgs {
    pub(crate) id: String,
    pub(crate) action_id: Option<String>,
    pub(crate) input_key: Option<String>,
}

pub(crate) fn encode_args(id: &str, action_id: Option<&str>, input_key: Option<&str>) -> String {
    let mut out = format!("id={}", percent_encode(id));
    if let Some(action_id) = action_id {
        out.push_str("&action=");
        out.push_str(&percent_encode(action_id));
    }
    if let Some(input_key) = input_key {
        out.push_str("&input=");
        out.push_str(&percent_encode(input_key));
    }
    out
}

pub(crate) fn decode_args(args: &str) -> Option<ToastArgs> {
    let mut decoded = ToastArgs {
        id: String::new(),
        action_id: None,
        input_key: None,
    };
    let mut has_id = false;
    for pair in args.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = percent_decode(value)?;
        match key {
            "id" => {
                decoded.id = value;
                has_id = true;
            }
            "action" => decoded.action_id = Some(value),
            "input" => decoded.input_key = Some(value),
            _ => {}
        }
    }
    has_id.then_some(decoded)
}

/// Makes an id usable as a toast tag or group.
pub(crate) fn tag_for(id: &str) -> String {
    if id.len() <= MAX_TAG_LEN {
        id.to_owned()
    } else {
        format!("{:032x}", stable_hash(id.as_bytes()))
    }
}

/// The toast's XML. Everything that can't be data bound lives here, so if this string is
/// unchanged an update can go through data binding, without re-creating the toast.
pub(crate) fn build(notification: &Notification, image_uri: Option<&str>) -> String {
    let id = &notification.id;
    let launch = encode_args(id, None, None);
    let has_actions = !notification.actions.is_empty();

    let mut root_attrs = String::new();
    if notification.persistent {
        // `reminder` keeps the toast on screen, but only when it has a button.
        root_attrs.push_str(if has_actions {
            r#" scenario="reminder""#
        } else {
            r#" duration="long""#
        });
    }
    if notification.actions.iter().any(|a| a.destructive) {
        root_attrs.push_str(r#" useButtonStyle="true""#);
    }

    let mut xml = format!(r#"<toast launch="{}"{root_attrs}>"#, escape(&launch));
    xml.push_str(r#"<visual><binding template="ToastGeneric">"#);
    xml.push_str(&format!("<text>{{{BIND_TITLE}}}</text>"));
    xml.push_str(&format!("<text>{{{BIND_BODY}}}</text>"));
    if let Some(subtitle) = &notification.subtitle {
        xml.push_str(&format!(
            r#"<text placement="attribution">{}</text>"#,
            escape(subtitle)
        ));
    }
    if let Some(uri) = image_uri {
        xml.push_str(&format!(
            r#"<image placement="appLogoOverride" src="{}"/>"#,
            escape(uri)
        ));
    }
    if notification.progress.is_some() {
        xml.push_str(&format!(
            r#"<progress value="{{{BIND_PROGRESS_VALUE}}}" valueStringOverride="{{{BIND_PROGRESS_LABEL}}}" status="{{{BIND_PROGRESS_STATUS}}}"/>"#
        ));
    }
    xml.push_str("</binding></visual>");

    if has_actions {
        xml.push_str("<actions>");
        // Windows requires every <input> to come before the first <action>.
        for (index, action) in notification.actions.iter().enumerate() {
            if let ActionKind::TextInput { placeholder, .. } = &action.kind {
                let hint = placeholder.as_deref().unwrap_or_default();
                xml.push_str(&format!(
                    r#"<input id="{}" type="text" placeHolderContent="{}"/>"#,
                    input_key(index),
                    escape(hint)
                ));
            }
        }
        for (index, action) in notification.actions.iter().enumerate() {
            let (input, label) = match &action.kind {
                ActionKind::TextInput { submit_label, .. } => (
                    Some(input_key(index)),
                    submit_label.as_deref().unwrap_or(&action.label),
                ),
                _ => (None, action.label.as_str()),
            };
            let args = encode_args(id, Some(&action.id), input.as_deref());
            let activation = if action.foreground {
                "foreground"
            } else {
                "background"
            };
            xml.push_str(&format!(
                r#"<action content="{}" arguments="{}" activationType="{activation}""#,
                escape(label),
                escape(&args)
            ));
            if let Some(input) = &input {
                xml.push_str(&format!(r#" hint-inputId="{input}""#));
            }
            if action.destructive {
                xml.push_str(r#" hint-buttonStyle="Critical""#);
            }
            xml.push_str("/>");
        }
        xml.push_str("</actions>");
    }

    match &notification.sound {
        Sound::Default => {}
        Sound::Silent => xml.push_str(r#"<audio silent="true"/>"#),
        Sound::Named(uri) => xml.push_str(&format!(r#"<audio src="{}"/>"#, escape(uri))),
    }
    // A low urgency toast never pops up, so don't let it make a sound either.
    if notification.urgency == Urgency::Low && notification.sound == Sound::Default {
        xml.push_str(r#"<audio silent="true"/>"#);
    }
    xml.push_str("</toast>");
    xml
}

/// The values for the data-bound placeholders in [`build`].
pub(crate) fn binding_values(notification: &Notification) -> Vec<(&'static str, String)> {
    let mut values = vec![
        (BIND_TITLE, notification.title.clone()),
        (BIND_BODY, notification.body.clone().unwrap_or_default()),
    ];
    if let Some(progress) = &notification.progress {
        let value = progress
            .clamped_value()
            .map_or_else(|| "indeterminate".to_owned(), |v| format!("{v:.4}"));
        let label = progress
            .label
            .clone()
            .or_else(|| progress.percent().map(|p| format!("{p}%")))
            .unwrap_or_default();
        // The schema requires a status, so use an invisible one rather than an empty string.
        let status = progress
            .status
            .clone()
            .unwrap_or_else(|| "\u{a0}".to_owned());
        values.push((BIND_PROGRESS_VALUE, value));
        values.push((BIND_PROGRESS_LABEL, label));
        values.push((BIND_PROGRESS_STATUS, status));
    }
    values
}

fn input_key(action_index: usize) -> String {
    format!("zn_input_{action_index}")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Other control characters are illegal in XML 1.0 and fail the whole toast.
            '\t' | '\n' | '\r' => out.push(c),
            c if u32::from(c) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A CLSID for the app's toast activator, derived from its id so it stays the same across
/// runs and installs. Formatted as Windows expects, with braces.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn activator_clsid(app_id: &str) -> u128 {
    let hash = stable_hash(format!("zephyr-notify-activator:{app_id}").as_bytes());
    // Mark it as an RFC 4122 version 4 style GUID so it can't collide with a time-based one.
    let versioned = (hash & !(0xf_u128 << 76)) | (0x4_u128 << 76);
    (versioned & !(0x3_u128 << 62)) | (0x2_u128 << 62)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn format_clsid(clsid: u128) -> String {
    let hex = format!("{clsid:032X}");
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Converts a Windows path to the `file:///` URI form toasts need.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn file_uri(path: &std::path::Path) -> Option<String> {
    let path = path.to_str()?.replace('\\', "/");
    let mut out = String::from("file:///");
    for byte in path.trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
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

    #[test]
    fn args_round_trip_with_hostile_values() {
        for id in ["simple", "a&b=c", "日本語 %41", "x\"y'<z>"] {
            let args = encode_args(id, Some("acc&ept"), Some("zn_input_0"));
            assert_eq!(
                decode_args(&args),
                Some(ToastArgs {
                    id: id.to_owned(),
                    action_id: Some("acc&ept".to_owned()),
                    input_key: Some("zn_input_0".to_owned()),
                })
            );
        }
        assert_eq!(
            decode_args(&encode_args("x", None, None))
                .unwrap()
                .action_id,
            None
        );
    }

    #[test]
    fn decode_rejects_malformed_args() {
        for bad in [
            "", "action=a", "id", "id=%", "id=%4", "id=%ZZ", "id=%FF", "id=é%",
        ] {
            assert_eq!(decode_args(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn peer_controlled_text_cannot_inject_xml() {
        let evil = r#""/><action content="pwn" arguments="x"/><a b=""#;
        let xml = build(
            &Notification::new("t")
                .id(evil)
                .subtitle("</text><image src='x'/>")
                .action(Action::button("ok", evil)),
            Some("file:///a&b"),
        );
        assert_eq!(xml.matches("<action ").count(), 1);
        assert!(!xml.contains("<image src"));
        assert!(xml.contains("file:///a&amp;b"));
    }

    #[test]
    fn title_and_body_are_bound_not_inlined() {
        let n = Notification::new("SECRET_TITLE").body("SECRET_BODY");
        let xml = build(&n, None);
        assert!(!xml.contains("SECRET"));
        assert!(xml.contains("<text>{zn_title}</text><text>{zn_body}</text>"));
        let values = binding_values(&n);
        assert!(values.contains(&(BIND_TITLE, "SECRET_TITLE".to_owned())));
    }

    #[test]
    fn progress_changes_do_not_change_the_xml() {
        let n = Notification::new("t")
            .id("i")
            .progress(Progress::fraction(0.1));
        let later = n.clone().progress(Progress::indeterminate().status("x"));
        assert_eq!(build(&n, None), build(&later, None));
    }

    #[test]
    fn progress_values() {
        let get = |p: Progress| {
            binding_values(&Notification::new("t").progress(p))
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        };
        let v = get(Progress::fraction(0.25));
        assert_eq!(v[BIND_PROGRESS_VALUE], "0.2500");
        assert_eq!(v[BIND_PROGRESS_LABEL], "25%");
        assert!(!v[BIND_PROGRESS_STATUS].is_empty());
        let v = get(Progress::indeterminate().status("Waiting").label("soon"));
        assert_eq!(v[BIND_PROGRESS_VALUE], "indeterminate");
        assert_eq!(v[BIND_PROGRESS_LABEL], "soon");
        assert_eq!(v[BIND_PROGRESS_STATUS], "Waiting");
    }

    #[test]
    fn inputs_come_before_actions_and_are_linked() {
        let xml = build(
            &Notification::new("t")
                .action(Action::button("no", "No").destructive())
                .action(Action::text_input("reply", "Reply").placeholder("Say something")),
            None,
        );
        assert!(xml.find("<input ").unwrap() < xml.find("<action ").unwrap());
        assert!(xml.contains(r#"<input id="zn_input_1""#));
        assert!(xml.contains(r#"hint-inputId="zn_input_1""#));
        assert!(xml.contains(r#"useButtonStyle="true""#));
        assert!(xml.contains(r#"hint-buttonStyle="Critical""#));
    }

    #[test]
    fn persistent_needs_buttons_to_use_reminder_scenario() {
        let plain = Notification::new("t").persistent(true);
        assert!(build(&plain, None).contains(r#"duration="long""#));
        let with_button = plain.action(Action::button("a", "A"));
        assert!(build(&with_button, None).contains(r#"scenario="reminder""#));
    }

    #[test]
    fn only_one_audio_element() {
        let n = Notification::new("t")
            .urgency(Urgency::Low)
            .sound(Sound::Silent);
        assert_eq!(build(&n, None).matches("<audio").count(), 1);
        let n = Notification::new("t").urgency(Urgency::Low);
        assert_eq!(build(&n, None).matches("<audio").count(), 1);
        assert_eq!(
            build(&Notification::new("t"), None)
                .matches("<audio")
                .count(),
            0
        );
    }

    #[test]
    fn long_ids_get_short_stable_tags() {
        assert_eq!(tag_for("short"), "short");
        let long = "x".repeat(200);
        assert!(tag_for(&long).len() <= MAX_TAG_LEN);
        assert_eq!(tag_for(&long), tag_for(&long));
    }

    #[test]
    fn control_chars_are_stripped() {
        assert_eq!(escape("a\u{1}b\nc"), "ab\nc");
    }

    #[test]
    fn clsid_is_stable_and_well_formed() {
        let clsid = activator_clsid("com.example.app");
        assert_eq!(clsid, activator_clsid("com.example.app"));
        assert_ne!(clsid, activator_clsid("com.example.other"));
        let text = format_clsid(clsid);
        assert_eq!(text.len(), 38);
        assert_eq!(&text[15..16], "4", "version nibble in {text}");
        assert!("89AB".contains(&text[20..21]), "variant nibble in {text}");
        assert_eq!(
            format_clsid(0x0123456789ABCDEF0123456789ABCDEF),
            "{01234567-89AB-CDEF-0123-456789ABCDEF}"
        );
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            file_uri(std::path::Path::new(r"C:\Users\a b\é.png")).unwrap(),
            "file:///C:/Users/a%20b/%C3%A9.png"
        );
    }
}
