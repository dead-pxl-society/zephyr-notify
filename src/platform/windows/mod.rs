//! Windows backend, built on WinRT toast notifications.
//!
//! Button presses arrive through a COM activator rather than the toast's `Activated`
//! event, because that's the only route that also works after the app has closed, for a
//! toast still sitting in Action Center. Registration lives under `HKCU` and doesn't need
//! the installer's Start Menu shortcut to be changed.

mod activator;

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    NotificationData, NotificationSetting, NotificationUpdateResult, ToastDismissalReason,
    ToastDismissedEventArgs, ToastFailedEventArgs, ToastNotification, ToastNotificationManager,
    ToastNotificationPriority, ToastNotifier,
};
use windows::core::{HSTRING, Ref};
use windows_registry::CURRENT_USER;

use crate::error::{Error, Result};
use crate::notification::{
    Capabilities, DismissReason, Event, Notification, Permission, ProgressSupport, Urgency,
};
use crate::notifier::{AppConfig, BackendApi, EventSink, Present, lock};
use crate::toast_xml;

/// Used when the notification doesn't set one, because toast updates need a group.
const DEFAULT_GROUP: &str = "zephyr-notify";

struct Shown {
    xml: String,
    group: String,
    /// Retained so its `Dismissed` subscription stays alive.
    _toast: ToastNotification,
}

#[derive(Default)]
pub(super) struct State {
    shown: HashMap<String, Shown>,
    sequence: u32,
}

pub(crate) struct WindowsBackend {
    app_id: HSTRING,
    notifier: ToastNotifier,
    sink: EventSink,
    state: Arc<Mutex<State>>,
    /// Dropping this unregisters the activator.
    _activator: Sender<()>,
}

impl BackendApi for WindowsBackend {
    fn new(config: &AppConfig, sink: EventSink) -> Result<Self> {
        let clsid = toast_xml::activator_clsid(&config.app_id);
        register_app(config, clsid)?;
        let state = Arc::new(Mutex::new(State::default()));
        let activator = activator::start(clsid, sink.clone(), state.clone())?;
        let app_id = HSTRING::from(&config.app_id);
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&app_id).map_err(backend_error)?;
        Ok(Self {
            app_id,
            notifier,
            sink,
            state,
            _activator: activator,
        })
    }

    fn show(&self, notification: &Notification, present: Present) -> Result<()> {
        let image = notification.image.as_deref().and_then(toast_xml::file_uri);
        let xml = toast_xml::build(notification, image.as_deref());
        let tag = toast_xml::tag_for(&notification.id);
        let group = toast_xml::tag_for(notification.group.as_deref().unwrap_or(DEFAULT_GROUP));

        let previous_group = {
            let state = lock(&self.state);
            let previous = state.shown.get(&notification.id);
            if present == Present::Silent
                && let Some(previous) = previous
                && previous.xml == xml
                && previous.group == group
            {
                drop(state);
                return self.update_in_place(notification, &tag, &group);
            }
            previous.map(|p| p.group.clone())
        };

        if present == Present::Alert && self.permission()? == Permission::Denied {
            return Err(Error::PermissionDenied);
        }
        // A tag is only unique within its group, so the old one wouldn't get replaced.
        if let Some(previous_group) = previous_group
            && previous_group != group
        {
            self.remove_from_history(&tag, &previous_group)?;
        }

        let document = XmlDocument::new().map_err(backend_error)?;
        document
            .LoadXml(&HSTRING::from(&xml))
            .map_err(backend_error)?;
        let toast = ToastNotification::CreateToastNotification(&document).map_err(backend_error)?;
        let configure = || -> windows::core::Result<()> {
            toast.SetTag(&HSTRING::from(&tag))?;
            toast.SetGroup(&HSTRING::from(&group))?;
            toast.SetData(&self.binding_data(notification)?)?;
            toast.SetSuppressPopup(
                present == Present::Silent || notification.urgency == Urgency::Low,
            )?;
            if notification.urgency == Urgency::High {
                toast.SetPriority(ToastNotificationPriority::High)?;
            }
            self.subscribe(&toast, &notification.id)?;
            self.notifier.Show(&toast)
        };
        configure().map_err(backend_error)?;

        lock(&self.state).shown.insert(
            notification.id.clone(),
            Shown {
                xml,
                group,
                _toast: toast,
            },
        );
        Ok(())
    }

    fn close(&self, id: &str) -> Result<()> {
        let tag = toast_xml::tag_for(id);
        let shown = lock(&self.state).shown.remove(id);
        match shown {
            Some(shown) => self.remove_from_history(&tag, &shown.group),
            None => self.remove_unknown_from_history(&tag),
        }
    }

    fn close_all(&self) -> Result<()> {
        lock(&self.state).shown.clear();
        history()?.ClearWithId(&self.app_id).map_err(backend_error)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            actions: true,
            text_input: true,
            progress: ProgressSupport::Native,
            images: true,
            sound: true,
            persistent: true,
            secondary_actions: true,
        }
    }

    fn permission(&self) -> Result<Permission> {
        let setting = self.notifier.Setting().map_err(backend_error)?;
        Ok(if setting == NotificationSetting::Enabled {
            Permission::Granted
        } else {
            Permission::Denied
        })
    }

    fn request_permission(&self) -> Result<Permission> {
        // Windows doesn't prompt. Users opt out in Settings.
        self.permission()
    }

    fn min_progress_interval(&self) -> Duration {
        Duration::from_millis(250)
    }
}

impl WindowsBackend {
    /// Changes the data-bound values of a toast that's already showing, without a popup.
    fn update_in_place(&self, notification: &Notification, tag: &str, group: &str) -> Result<()> {
        let data = self.binding_data(notification).map_err(backend_error)?;
        let result = self
            .notifier
            .UpdateWithTagAndGroup(&data, &HSTRING::from(tag), &HSTRING::from(group))
            .map_err(backend_error)?;
        if result == NotificationUpdateResult::NotificationNotFound {
            lock(&self.state).shown.remove(&notification.id);
        } else if result != NotificationUpdateResult::Succeeded {
            return Err(Error::Backend("toast update failed".into()));
        }
        Ok(())
    }

    fn binding_data(&self, notification: &Notification) -> windows::core::Result<NotificationData> {
        let data = NotificationData::new()?;
        let values = data.Values()?;
        for (key, value) in toast_xml::binding_values(notification) {
            values.Insert(&HSTRING::from(key), &HSTRING::from(value))?;
        }
        // Windows ignores an update whose sequence number isn't higher than the last.
        let sequence = {
            let mut state = lock(&self.state);
            state.sequence = state.sequence.wrapping_add(1).max(1);
            state.sequence
        };
        data.SetSequenceNumber(sequence)?;
        Ok(data)
    }

    fn subscribe(&self, toast: &ToastNotification, id: &str) -> windows::core::Result<()> {
        let dismissed_id = id.to_owned();
        let sink = self.sink.clone();
        let state = self.state.clone();
        toast.Dismissed(&TypedEventHandler::new(
            move |_: Ref<ToastNotification>, args: Ref<ToastDismissedEventArgs>| {
                // A timeout only moves it to Action Center, where it remains usable, and
                // `ApplicationHidden` is our own close or replace.
                if let Some(args) = args.as_ref()
                    && args.Reason()? == ToastDismissalReason::UserCanceled
                {
                    lock(&state).shown.remove(&dismissed_id);
                    sink.emit(Event::Dismissed {
                        id: dismissed_id.clone(),
                        reason: DismissReason::User,
                    });
                }
                Ok(())
            },
        ))?;
        let failed_id = id.to_owned();
        toast.Failed(&TypedEventHandler::new(
            move |_: Ref<ToastNotification>, args: Ref<ToastFailedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    log::warn!(
                        "Windows failed to show notification {failed_id}: {:?}",
                        args.ErrorCode()
                    );
                }
                Ok(())
            },
        ))?;
        Ok(())
    }

    fn remove_from_history(&self, tag: &str, group: &str) -> Result<()> {
        history()?
            .RemoveGroupedTagWithId(&HSTRING::from(tag), &HSTRING::from(group), &self.app_id)
            .map_err(backend_error)
    }

    /// Handles an id from an earlier run, where only Windows knows the group.
    fn remove_unknown_from_history(&self, tag: &str) -> Result<()> {
        let history = history()?;
        let toasts = history
            .GetHistoryWithId(&self.app_id)
            .map_err(backend_error)?;
        for toast in toasts {
            if toast.Tag().is_ok_and(|t| t == *tag) {
                let group = toast.Group().map_err(backend_error)?;
                history
                    .RemoveGroupedTagWithId(&HSTRING::from(tag), &group, &self.app_id)
                    .map_err(backend_error)?;
            }
        }
        Ok(())
    }
}

fn history() -> Result<windows::UI::Notifications::ToastNotificationHistory> {
    ToastNotificationManager::History().map_err(backend_error)
}

fn aumid_key(app_id: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{app_id}")
}

fn clsid_key(clsid: u128) -> String {
    format!(r"Software\Classes\CLSID\{}", toast_xml::format_clsid(clsid))
}

/// Tells Windows how to show the app's name and icon, and which exe handles button presses.
/// Rewritten on each start so it follows the exe if the app moves.
fn register_app(config: &AppConfig, clsid: u128) -> Result<()> {
    let exe = std::env::current_exe().map_err(backend_error)?;
    let exe = exe
        .to_str()
        .ok_or_else(|| Error::Backend("executable path is not valid Unicode".into()))?;

    let app = CURRENT_USER
        .create(aumid_key(&config.app_id))
        .map_err(backend_error)?;
    app.set_string("DisplayName", &config.app_name)
        .map_err(backend_error)?;
    app.set_string("CustomActivator", toast_xml::format_clsid(clsid))
        .map_err(backend_error)?;
    match config.icon.as_deref().and_then(std::path::Path::to_str) {
        Some(icon) => app.set_string("IconUri", icon).map_err(backend_error)?,
        // The value may be stale from a previous run, and it's fine if it isn't there.
        None => drop(app.remove_value("IconUri")),
    }

    CURRENT_USER
        .create(format!(r"{}\LocalServer32", clsid_key(clsid)))
        .map_err(backend_error)?
        .set_string("", format!("\"{exe}\""))
        .map_err(backend_error)
}

/// Removes everything [`Notifier::build`](crate::NotifierBuilder::build) wrote to the
/// registry. Call this from your uninstaller, because the entries live in `HKCU` and
/// outlast the app.
pub fn unregister_windows_app(app_id: &str) -> Result<()> {
    let clsid = toast_xml::activator_clsid(app_id);
    for key in [aumid_key(app_id), clsid_key(clsid)] {
        if let Err(error) = CURRENT_USER.remove_tree(&key) {
            // Not being registered is what the caller wanted anyway.
            log::debug!("couldn't remove registry key {key}: {error}");
        }
    }
    Ok(())
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(error.to_string())
}

/// Runs `f` on a new thread and waits for its first result, leaving the thread running.
pub(super) fn spawn_with_ready<T: Send + 'static>(
    name: &str,
    f: impl FnOnce(Sender<Result<T>>) + Send + 'static,
) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || f(tx))
        .map_err(backend_error)?;
    rx.recv()
        .map_err(|_| Error::Backend("activator thread stopped".into()))?
}
