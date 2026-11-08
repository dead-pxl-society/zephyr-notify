//! macOS backend, built on `UNUserNotificationCenter`.
//!
//! macOS has no notification progress bar, so progress is written into the body as text.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, available, define_class, msg_send};
use objc2_foundation::{NSArray, NSBundle, NSError, NSSet, NSString, NSURL};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNErrorCode, UNMutableNotificationContent,
    UNNotification, UNNotificationAction, UNNotificationActionOptions, UNNotificationAttachment,
    UNNotificationCategory, UNNotificationCategoryOptions, UNNotificationDefaultActionIdentifier,
    UNNotificationDismissActionIdentifier, UNNotificationInterruptionLevel,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNNotificationSound, UNTextInputNotificationAction,
    UNTextInputNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use crate::error::{Error, Result};
use crate::notification::{
    Action, ActionKind, Capabilities, DismissReason, Event, Notification, Permission,
    ProgressSupport, Sound, Urgency, stable_hash,
};
use crate::notifier::{AppConfig, BackendApi, EventSink, Present, lock};

/// Longest wait for the notification service to answer, except for permission prompts.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// `setNotificationCategories` applies asynchronously, and a notification posted before
/// it lands silently has no buttons. These bound the wait for it to land.
const CATEGORY_POLL_ATTEMPTS: u32 = 20;
const CATEGORY_POLL_INTERVAL: Duration = Duration::from_millis(50);

const CATEGORY_PREFIX: &str = "zephyr-notify.";

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "ZephyrNotifyDelegate"]
    #[ivars = EventSink]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // Called when a notification arrives while the app is frontmost. Without this
        // macOS would hide it, and a transfer request would go unnoticed.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            notification: &UNNotification,
            handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((presentation_options(notification),));
        }

        // Called for any response, possibly right after the response launched the app.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            handler: &DynBlock<dyn Fn()>,
        ) {
            self.ivars().emit(event_from_response(response));
            handler.call(());
        }
    }
);

/// The notification center holds its delegate weakly, so it's retained here.
static DELEGATE: Mutex<Option<AssertThreadSafe<Retained<Delegate>>>> = Mutex::new(None);

/// For Objective-C objects that are documented as thread safe or immutable but
/// aren't marked `Send` by the bindings.
struct AssertThreadSafe<T>(T);
// SAFETY: only used for the delegate, which holds only `Send + Sync` state, and
// for `UNNotificationCategory`, which is immutable.
unsafe impl<T> Send for AssertThreadSafe<T> {}
unsafe impl<T> Sync for AssertThreadSafe<T> {}

fn presentation_options(notification: &UNNotification) -> UNNotificationPresentationOptions {
    use UNNotificationPresentationOptions as Options;
    if !available!(macos = 11.0) {
        #[allow(deprecated)] // Its replacements don't exist before macOS 11.
        return Options::Alert | Options::Sound;
    }
    if available!(macos = 12.0)
        && notification.request().content().interruptionLevel()
            == UNNotificationInterruptionLevel::Passive
    {
        // A progress tick or low urgency: update the list but don't pop a banner.
        return Options::List;
    }
    Options::Banner | Options::List | Options::Sound
}

fn event_from_response(response: &UNNotificationResponse) -> Event {
    let id = response.notification().request().identifier().to_string();
    let action = response.actionIdentifier();
    // SAFETY: these are constant strings provided by the framework.
    let (default, dismiss) = unsafe {
        (
            UNNotificationDefaultActionIdentifier,
            UNNotificationDismissActionIdentifier,
        )
    };
    if &*action == default {
        Event::Activated { id }
    } else if &*action == dismiss {
        Event::Dismissed {
            id,
            reason: DismissReason::User,
        }
    } else {
        let input = response
            .downcast_ref::<UNTextInputNotificationResponse>()
            .map(|r| r.userText().to_string());
        Event::Action {
            id,
            action_id: action.to_string(),
            input,
        }
    }
}

pub(crate) struct MacBackend {
    /// Categories known to be registered with the OS.
    categories: Mutex<HashSet<String>>,
}

impl BackendApi for MacBackend {
    fn new(_config: &AppConfig, sink: EventSink) -> Result<Self> {
        ensure_bundled()?;
        if !available!(macos = 10.14) {
            return Err(Error::Unsupported);
        }
        let delegate = Delegate::alloc().set_ivars(sink);
        // SAFETY: `NSObject`'s `init` has no preconditions.
        let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
        center().setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        *lock(&DELEGATE) = Some(AssertThreadSafe(delegate));
        Ok(Self {
            categories: Mutex::new(HashSet::new()),
        })
    }

    fn show(&self, notification: &Notification, present: Present) -> Result<()> {
        // Without permission the OS never applies categories, so fail here with a clear
        // error instead of waiting on them.
        if self.permission()? != Permission::Granted {
            return Err(Error::PermissionDenied);
        }
        let category = self.ensure_category(notification)?;
        let content = build_content(notification, present, &category);
        let id = NSString::from_str(&notification.id);
        // A nil trigger means deliver now.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &id,
            &content.into_super(),
            None,
        );

        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: the framework passes nil or a valid error.
            let _ = tx.send(unsafe { error.as_ref() }.map(ns_error_to_error));
        });
        center().addNotificationRequest_withCompletionHandler(&request, Some(&block));
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(None) => Ok(()),
            Ok(Some(error)) => Err(error),
            Err(_) => Err(timeout_error()),
        }
    }

    fn close(&self, id: &str) -> Result<()> {
        let ids = NSArray::from_retained_slice(&[NSString::from_str(id)]);
        let center = center();
        center.removePendingNotificationRequestsWithIdentifiers(&ids);
        center.removeDeliveredNotificationsWithIdentifiers(&ids);
        Ok(())
    }

    fn close_all(&self) -> Result<()> {
        let center = center();
        center.removeAllPendingNotificationRequests();
        center.removeAllDeliveredNotifications();
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            actions: true,
            text_input: true,
            progress: ProgressSupport::Text,
            images: true,
            sound: true,
            persistent: false,
        }
    }

    fn permission(&self) -> Result<Permission> {
        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: the framework passes a valid object.
            let _ = tx.send(unsafe { settings.as_ref() }.authorizationStatus());
        });
        center().getNotificationSettingsWithCompletionHandler(&block);
        let status = rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| timeout_error())?;
        Ok(match status {
            UNAuthorizationStatus::NotDetermined => Permission::NotDetermined,
            UNAuthorizationStatus::Denied => Permission::Denied,
            _ => Permission::Granted,
        })
    }

    fn request_permission(&self) -> Result<Permission> {
        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
            // SAFETY: the framework passes nil or a valid error.
            let _ = tx.send(unsafe { error.as_ref() }.map(ns_error_to_error));
        });
        let options = UNAuthorizationOptions::Alert
            | UNAuthorizationOptions::Sound
            | UNAuthorizationOptions::Badge;
        center().requestAuthorizationWithOptions_completionHandler(options, &block);
        // No timeout: this waits for the user to answer the prompt.
        match rx.recv() {
            Ok(None) => {}
            // Denied isn't a failure of this call. The final state is reported below.
            Ok(Some(Error::PermissionDenied)) => return Ok(Permission::Denied),
            Ok(Some(error)) => return Err(error),
            Err(_) => return Err(timeout_error()),
        }
        self.permission()
    }

    fn min_progress_interval(&self) -> Duration {
        // Each update replaces the whole notification, so keep them infrequent.
        Duration::from_secs(1)
    }
}

impl MacBackend {
    /// Registers the category holding this notification's buttons and returns its id.
    /// Every category asks for dismiss callbacks, including the one with no buttons.
    fn ensure_category(&self, notification: &Notification) -> Result<String> {
        let id = category_id(&notification.actions);
        let mut known = lock(&self.categories);
        if known.contains(&id) {
            return Ok(id);
        }

        // Setting categories replaces all of them. Keep the existing ones so that
        // buttons on notifications from an earlier run still work.
        let mut categories: Vec<_> = fetch_categories()?.into_iter().map(|c| c.0).collect();
        let already_set = categories.iter().any(|c| c.identifier().to_string() == id);
        if !already_set {
            categories.push(build_category(&id, &notification.actions));
            center().setNotificationCategories(&NSSet::from_retained_slice(&categories));
            if !wait_for_category(&id)? {
                // Still post it, since a notification missing its buttons beats none. It
                // isn't cached, so the next notification tries again.
                log::warn!(
                    "notification actions weren't confirmed by macOS; buttons may be missing"
                );
                return Ok(id);
            }
        }
        known.insert(id.clone());
        Ok(id)
    }
}

/// Returns whether the category landed within the time limit.
fn wait_for_category(id: &str) -> Result<bool> {
    for _ in 0..CATEGORY_POLL_ATTEMPTS {
        let present = fetch_categories()?
            .iter()
            .any(|c| c.0.identifier().to_string() == id);
        if present {
            return Ok(true);
        }
        std::thread::sleep(CATEGORY_POLL_INTERVAL);
    }
    Ok(false)
}

fn fetch_categories() -> Result<Vec<AssertThreadSafe<Retained<UNNotificationCategory>>>> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |set: NonNull<NSSet<UNNotificationCategory>>| {
        // SAFETY: the framework passes a valid set.
        let items = unsafe { set.as_ref() }
            .iter()
            .map(AssertThreadSafe)
            .collect();
        let _ = tx.send(items);
    });
    center().getNotificationCategoriesWithCompletionHandler(&block);
    rx.recv_timeout(REPLY_TIMEOUT).map_err(|_| timeout_error())
}

fn build_category(id: &str, actions: &[Action]) -> Retained<UNNotificationCategory> {
    let actions: Vec<_> = actions.iter().map(build_action).collect();
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(id),
        &NSArray::from_retained_slice(&actions),
        &NSArray::new(),
        UNNotificationCategoryOptions::CustomDismissAction,
    )
}

fn build_action(action: &Action) -> Retained<UNNotificationAction> {
    let mut options = UNNotificationActionOptions::empty();
    if action.destructive {
        options |= UNNotificationActionOptions::Destructive;
    }
    if action.foreground {
        options |= UNNotificationActionOptions::Foreground;
    }
    let id = NSString::from_str(&action.id);
    let label = NSString::from_str(&action.label);
    match &action.kind {
        ActionKind::TextInput {
            placeholder,
            submit_label,
        } => UNTextInputNotificationAction::actionWithIdentifier_title_options_textInputButtonTitle_textInputPlaceholder(
            &id,
            &label,
            options,
            &NSString::from_str(submit_label.as_deref().unwrap_or(&action.label)),
            &NSString::from_str(placeholder.as_deref().unwrap_or_default()),
        )
        .into_super(),
        _ => UNNotificationAction::actionWithIdentifier_title_options(&id, &label, options),
    }
}

/// A category id determined entirely by the buttons, so identical sets share a category
/// across runs. Categories accumulate in the OS, so don't put per-transfer values such as
/// file names in action labels.
fn category_id(actions: &[Action]) -> String {
    let mut key = String::new();
    for action in actions {
        let (placeholder, submit) = match &action.kind {
            ActionKind::TextInput {
                placeholder,
                submit_label,
            } => (placeholder.as_deref(), submit_label.as_deref()),
            _ => (None, None),
        };
        let is_input = matches!(action.kind, ActionKind::TextInput { .. });
        key.push_str(&format!(
            "{:?}\0{:?}\0{is_input}\0{placeholder:?}\0{submit:?}\0{}\0{}\0",
            action.id, action.label, action.destructive, action.foreground
        ));
    }
    format!("{CATEGORY_PREFIX}{:032x}", stable_hash(key.as_bytes()))
}

fn build_content(
    notification: &Notification,
    present: Present,
    category: &str,
) -> Retained<UNMutableNotificationContent> {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notification.title));
    if let Some(subtitle) = &notification.subtitle {
        content.setSubtitle(&NSString::from_str(subtitle));
    }
    content.setBody(&NSString::from_str(&notification.body_with_progress_text()));
    content.setCategoryIdentifier(&NSString::from_str(category));
    if let Some(group) = &notification.group {
        content.setThreadIdentifier(&NSString::from_str(group));
    }

    let quiet = present == Present::Silent || notification.urgency == Urgency::Low;
    let sound = match &notification.sound {
        _ if quiet => None,
        Sound::Silent => None,
        Sound::Named(name) => Some(UNNotificationSound::soundNamed(&NSString::from_str(name))),
        _ => Some(UNNotificationSound::defaultSound()),
    };
    content.setSound(sound.as_deref());

    if available!(macos = 12.0) {
        content.setInterruptionLevel(if quiet {
            UNNotificationInterruptionLevel::Passive
        } else if notification.urgency == Urgency::High {
            UNNotificationInterruptionLevel::TimeSensitive
        } else {
            UNNotificationInterruptionLevel::Active
        });
    }

    if let Some(attachment) = notification.image.as_deref().and_then(build_attachment) {
        content.setAttachments(&NSArray::from_retained_slice(&[attachment]));
    }
    content
}

/// A bad image shouldn't cost the user the notification, so failures only log.
fn build_attachment(path: &Path) -> Option<Retained<UNNotificationAttachment>> {
    // The OS moves the attachment into its own storage, so give it a private link to the
    // file. Otherwise it would take the user's original.
    let temp = private_copy(path)
        .inspect_err(|e| log::warn!("can't prepare notification image {path:?}: {e}"))
        .ok()?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(temp.to_str()?));
    // SAFETY: passing no options has no type requirements.
    let result = unsafe {
        UNNotificationAttachment::attachmentWithIdentifier_URL_options_error(
            &NSString::from_str("image"),
            &url,
            None,
        )
    };
    match result {
        Ok(attachment) => Some(attachment),
        Err(error) => {
            log::warn!(
                "macOS rejected notification image {path:?}: {}",
                error.localizedDescription()
            );
            let _ = std::fs::remove_file(&temp);
            None
        }
    }
}

fn private_copy(path: &Path) -> std::io::Result<std::path::PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join("zephyr-notify");
    std::fs::create_dir_all(&dir)?;
    // macOS picks the attachment type from the extension, so it must be preserved.
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or("png");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!("{}-{n}.{extension}", std::process::id()));
    // A hard link makes repeat updates cheap even for a large file.
    if std::fs::hard_link(path, &temp).is_err() {
        std::fs::copy(path, &temp)?;
    }
    Ok(temp)
}

/// `currentNotificationCenter` raises an uncatchable exception outside an app bundle, so
/// this has to run first.
fn ensure_bundled() -> Result<()> {
    let bundle = NSBundle::mainBundle();
    let has_id = bundle.bundleIdentifier().is_some();
    let is_app = bundle.bundlePath().to_string().ends_with(".app");
    if has_id && is_app {
        Ok(())
    } else {
        Err(Error::NotBundled)
    }
}

fn center() -> Retained<UNUserNotificationCenter> {
    UNUserNotificationCenter::currentNotificationCenter()
}

fn ns_error_to_error(error: &NSError) -> Error {
    if error.code() == UNErrorCode::NotificationsNotAllowed.0 {
        Error::PermissionDenied
    } else {
        Error::Backend(error.localizedDescription().to_string())
    }
}

fn timeout_error() -> Error {
    Error::Backend("timed out waiting for the notification service".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_id_depends_only_on_buttons() {
        let a = [Action::button("a", "A")];
        assert_eq!(category_id(&a), category_id(&[Action::button("a", "A")]));
        assert_ne!(category_id(&a), category_id(&[Action::button("a", "B")]));
        assert_ne!(
            category_id(&a),
            category_id(&[Action::button("a", "A").destructive()])
        );
        assert_ne!(
            category_id(&a),
            category_id(&[Action::text_input("a", "A")])
        );
        assert_ne!(category_id(&a), category_id(&[]));
    }

    #[test]
    fn private_copy_leaves_the_original_in_place() {
        let dir = std::env::temp_dir().join(format!("zn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("a.jpeg");
        std::fs::write(&original, b"x").unwrap();
        let copy = private_copy(&original).unwrap();
        assert_eq!(copy.extension().unwrap(), "jpeg");
        std::fs::remove_file(&copy).unwrap();
        assert!(original.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
