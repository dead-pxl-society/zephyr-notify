//! The COM server Windows calls when the user presses a toast or one of its buttons.

use std::ffi::c_void;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_NOINTERFACE};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
    CoRevokeClassObject, CoUninitialize, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};
use windows::core::{BOOL, GUID, IUnknown, Interface, PCWSTR, Ref, Result as WinResult, implement};

use super::{State, spawn_with_ready};
use crate::error::{Error, Result};
use crate::notification::Event;
use crate::notifier::{EventSink, lock};
use crate::toast_xml::{self, ToastArgs};

#[implement(INotificationActivationCallback)]
struct Activator {
    sink: EventSink,
    state: Arc<Mutex<State>>,
}

impl INotificationActivationCallback_Impl for Activator_Impl {
    fn Activate(
        &self,
        _app_user_model_id: &PCWSTR,
        invoked_args: &PCWSTR,
        data: *const NOTIFICATION_USER_INPUT_DATA,
        count: u32,
    ) -> WinResult<()> {
        // SAFETY: Windows passes a valid string, and `count` valid entries in `data`.
        let args = unsafe { invoked_args.to_string() }?;
        let Some(args) = toast_xml::decode_args(&args) else {
            log::warn!("ignoring toast activation with unrecognized arguments");
            return Ok(());
        };
        let inputs = if data.is_null() || count == 0 {
            &[][..]
        } else {
            // SAFETY: as above.
            unsafe { std::slice::from_raw_parts(data, count as usize) }
        };
        let input = read_input(&args, inputs);
        lock(&self.state).shown.remove(&args.id);
        self.sink.emit(match args.action_id {
            None => Event::Activated { id: args.id },
            Some(action_id) => Event::Action {
                id: args.id,
                action_id,
                input,
            },
        });
        Ok(())
    }
}

fn read_input(args: &ToastArgs, inputs: &[NOTIFICATION_USER_INPUT_DATA]) -> Option<String> {
    let wanted = args.input_key.as_deref()?;
    inputs.iter().find_map(|entry| {
        // SAFETY: Windows passes valid strings in each entry.
        let key = unsafe { entry.Key.to_string() }.ok()?;
        if key == wanted {
            // SAFETY: as above.
            unsafe { entry.Value.to_string() }.ok()
        } else {
            None
        }
    })
}

#[implement(IClassFactory)]
struct Factory {
    sink: EventSink,
    state: Arc<Mutex<State>>,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> WinResult<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_NOINTERFACE.into());
        }
        // SAFETY: checked non-null above, and COM guarantees a valid out pointer.
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: INotificationActivationCallback = Activator {
            sink: self.sink.clone(),
            state: self.state.clone(),
        }
        .into();
        // SAFETY: `iid` and `object` were checked above.
        unsafe { activator.query(iid, object) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> WinResult<()> {
        Ok(())
    }
}

/// Registers the activator on a dedicated multithreaded-apartment thread, independent of
/// the apartment the app's main thread uses. It stays registered until the returned
/// sender is dropped.
pub(super) fn start(clsid: u128, sink: EventSink, state: Arc<Mutex<State>>) -> Result<Sender<()>> {
    spawn_with_ready("zephyr-notify-activator", move |ready| {
        // SAFETY: balanced by the `CoUninitialize` below, on this same thread.
        if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
            let _ = ready.send(Err(Error::Backend(error.to_string())));
            return;
        }
        let factory: IClassFactory = Factory { sink, state }.into();
        let clsid = GUID::from_u128(clsid);
        // SAFETY: `factory` is a valid class object, and COM holds a reference to it.
        let cookie = unsafe {
            CoRegisterClassObject(&clsid, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE)
        };
        match cookie {
            Ok(cookie) => {
                let (stop_tx, stop_rx) = mpsc::channel::<()>();
                let _ = ready.send(Ok(stop_tx));
                // Parks until the backend is dropped. The apartment must stay alive to
                // keep receiving activations.
                let _ = stop_rx.recv();
                // SAFETY: `cookie` came from the registration above.
                if let Err(error) = unsafe { CoRevokeClassObject(cookie) } {
                    log::warn!("couldn't unregister toast activator: {error}");
                }
            }
            Err(error) => {
                let _ = ready.send(Err(Error::Backend(error.to_string())));
            }
        }
        // SAFETY: balances the `CoInitializeEx` above.
        unsafe { CoUninitialize() };
    })
}
