//! Linux and BSD backend, speaking `org.freedesktop.Notifications` over D-Bus.
//!
//! All blocking D-Bus calls run on plain threads owned by this module. If anything in the
//! app enables zbus's `tokio` feature, `zbus::blocking` panics when called on a tokio
//! thread, and a Tauri command would be exactly that.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::Value;
use zbus::{MatchRule, Message};

use crate::error::{Error, Result};
use crate::fdo::{self, Hint, ServerInfo};
use crate::notification::{Capabilities, Event, Notification, Permission};
use crate::notifier::{AppConfig, BackendApi, EventSink, Present, lock};

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const INTERFACE: &str = "org.freedesktop.Notifications";

type Job = Box<dyn FnOnce(&Worker) + Send>;

pub(crate) struct LinuxBackend {
    jobs: Sender<Job>,
    capabilities: Capabilities,
}

#[derive(Default)]
struct Registry {
    /// Our id to the server's current id. A notification's server id can change on replace.
    server_ids: HashMap<String, u32>,
    entries: HashMap<u32, Entry>,
}

struct Entry {
    id: String,
    reply_action: Option<String>,
}

impl Registry {
    fn insert(&mut self, id: &str, server_id: u32, reply_action: Option<String>) {
        if let Some(previous) = self.server_ids.insert(id.to_owned(), server_id) {
            self.entries.remove(&previous);
        }
        self.entries.insert(
            server_id,
            Entry {
                id: id.to_owned(),
                reply_action,
            },
        );
    }

    fn remove_server_id(&mut self, server_id: u32) -> Option<Entry> {
        let entry = self.entries.remove(&server_id)?;
        self.server_ids.remove(&entry.id);
        Some(entry)
    }
}

struct Worker {
    connection: Connection,
    server: ServerInfo,
    app_id: String,
    app_name: String,
    icon: Option<PathBuf>,
    registry: Arc<Mutex<Registry>>,
}

impl Worker {
    fn show(&self, notification: &Notification, present: Present) -> Result<()> {
        let replaces = lock(&self.registry)
            .server_ids
            .get(&notification.id)
            .copied();
        if present == Present::Silent && replaces.is_none() {
            // It was closed, and replacing would reopen it with a new banner.
            return Ok(());
        }
        let request = fdo::build_request(
            notification,
            &self.server,
            &self.app_id,
            self.icon.as_deref(),
            present,
        );
        let hints: HashMap<&str, Value<'_>> = request.hints.iter().map(hint_entry).collect();
        let reply = self
            .connection
            .call_method(
                Some(DESTINATION),
                PATH,
                Some(INTERFACE),
                "Notify",
                &(
                    &self.app_name,
                    replaces.unwrap_or(0),
                    &request.app_icon,
                    &request.summary,
                    &request.body,
                    &request.actions,
                    hints,
                    request.expire_timeout,
                ),
            )
            .map_err(backend_error)?;
        let server_id: u32 = reply.body().deserialize().map_err(backend_error)?;
        lock(&self.registry).insert(&notification.id, server_id, request.reply_action);
        Ok(())
    }

    fn close(&self, id: &str) -> Result<()> {
        let server_id = {
            let mut registry = lock(&self.registry);
            let server_id = registry.server_ids.get(id).copied();
            if let Some(server_id) = server_id {
                registry.remove_server_id(server_id);
            }
            server_id
        };
        match server_id {
            Some(server_id) => close_on_server(&self.connection, server_id),
            None => Ok(()),
        }
    }

    fn close_all(&self) -> Result<()> {
        let server_ids: Vec<u32> = {
            let mut registry = lock(&self.registry);
            registry.server_ids.clear();
            registry
                .entries
                .drain()
                .map(|(server_id, _)| server_id)
                .collect()
        };
        server_ids
            .into_iter()
            .try_for_each(|server_id| close_on_server(&self.connection, server_id))
    }
}

fn close_on_server(connection: &Connection, server_id: u32) -> Result<()> {
    connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(INTERFACE),
            "CloseNotification",
            &server_id,
        )
        .map(drop)
        .map_err(backend_error)
}

fn hint_entry(hint: &Hint) -> (&str, Value<'_>) {
    match hint {
        Hint::Str(key, value) => (*key, Value::from(value.as_str())),
        Hint::Byte(key, value) => (*key, Value::from(*value)),
        Hint::Int(key, value) => (*key, Value::from(*value)),
        Hint::Bool(key, value) => (*key, Value::from(*value)),
    }
}

fn connect(config: &AppConfig) -> Result<(Worker, MessageIterator)> {
    let connection = Connection::session().map_err(backend_error)?;
    // Subscribed before anything is shown, so no signal can be missed.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INTERFACE)
        .map_err(backend_error)?
        .path(PATH)
        .map_err(backend_error)?
        .build();
    let signals =
        MessageIterator::for_match_rule(rule, &connection, None).map_err(backend_error)?;
    let server = query_server(&connection)?;
    let worker = Worker {
        connection,
        server,
        app_id: config.app_id.clone(),
        app_name: config.app_name.clone(),
        icon: config.icon.clone(),
        registry: Arc::default(),
    };
    Ok((worker, signals))
}

fn query_server(connection: &Connection) -> Result<ServerInfo> {
    let call = |method: &str| {
        connection
            .call_method(Some(DESTINATION), PATH, Some(INTERFACE), method, &())
            .map_err(backend_error)
    };
    let capabilities: Vec<String> = call("GetCapabilities")?
        .body()
        .deserialize()
        .map_err(backend_error)?;
    let (name, ..): (String, String, String, String) = call("GetServerInformation")?
        .body()
        .deserialize()
        .map_err(backend_error)?;
    Ok(ServerInfo {
        name,
        capabilities: capabilities.into_iter().collect(),
    })
}

fn listen(
    signals: MessageIterator,
    connection: Connection,
    registry: Arc<Mutex<Registry>>,
    sink: EventSink,
) {
    for message in signals {
        let Ok(message) = message else { continue };
        if let Err(error) = handle_signal(&message, &connection, &registry, &sink) {
            log::warn!("ignoring malformed notification signal: {error}");
        }
    }
}

fn handle_signal(
    message: &Message,
    connection: &Connection,
    registry: &Mutex<Registry>,
    sink: &EventSink,
) -> zbus::Result<()> {
    let header = message.header();
    let Some(member) = header.member() else {
        return Ok(());
    };
    match member.as_str() {
        "ActionInvoked" => {
            let (server_id, key): (u32, String) = message.body().deserialize()?;
            if key == fdo::INLINE_REPLY_ACTION {
                return Ok(()); // The reply itself arrives as `NotificationReplied`.
            }
            // Signals are broadcast to all clients, so this filters out other apps'.
            let Some(entry) = lock(registry).remove_server_id(server_id) else {
                return Ok(());
            };
            // Servers may leave it on screen after an action. It's now finished, so make
            // that match macOS and Windows.
            if let Err(error) = close_on_server(connection, server_id) {
                log::debug!("couldn't close notification after action: {error}");
            }
            sink.emit(if key == fdo::DEFAULT_ACTION {
                Event::Activated { id: entry.id }
            } else {
                Event::Action {
                    id: entry.id,
                    action_id: key,
                    input: None,
                }
            });
        }
        "NotificationReplied" => {
            let (server_id, text): (u32, String) = message.body().deserialize()?;
            let Some(entry) = lock(registry).remove_server_id(server_id) else {
                return Ok(());
            };
            if let Some(action_id) = entry.reply_action {
                sink.emit(Event::Action {
                    id: entry.id,
                    action_id,
                    input: Some(text),
                });
            }
        }
        "NotificationClosed" => {
            let (server_id, reason): (u32, u32) = message.body().deserialize()?;
            let Some(entry) = lock(registry).remove_server_id(server_id) else {
                return Ok(());
            };
            if let Some(reason) = fdo::dismiss_reason(reason) {
                sink.emit(Event::Dismissed {
                    id: entry.id,
                    reason,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

impl LinuxBackend {
    fn run<T: Send + 'static>(&self, job: impl FnOnce(&Worker) -> T + Send + 'static) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Box::new(move |worker| {
                let _ = tx.send(job(worker));
            }))
            .map_err(|_| worker_gone())?;
        rx.recv().map_err(|_| worker_gone())
    }
}

impl BackendApi for LinuxBackend {
    fn new(config: &AppConfig, sink: EventSink) -> Result<Self> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let config = config.clone();
        thread::Builder::new()
            .name("zephyr-notify-dbus".into())
            .spawn(move || {
                let (worker, signals) = match connect(&config) {
                    Ok(connected) => connected,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let listener = {
                    let connection = worker.connection.clone();
                    let registry = worker.registry.clone();
                    // Stays blocked on the bus until the process exits.
                    thread::Builder::new()
                        .name("zephyr-notify-signals".into())
                        .spawn(move || listen(signals, connection, registry, sink))
                };
                if let Err(error) = listener {
                    let _ = ready_tx.send(Err(backend_error(error)));
                    return;
                }
                let _ = ready_tx.send(Ok(worker.server.to_capabilities()));
                while let Ok(job) = job_rx.recv() {
                    job(&worker);
                }
            })
            .map_err(backend_error)?;
        let capabilities = ready_rx.recv().map_err(|_| worker_gone())??;
        Ok(Self { jobs, capabilities })
    }

    fn show(&self, notification: &Notification, present: Present) -> Result<()> {
        let notification = notification.clone();
        self.run(move |worker| worker.show(&notification, present))?
    }

    fn close(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.run(move |worker| worker.close(&id))?
    }

    fn close_all(&self) -> Result<()> {
        self.run(Worker::close_all)?
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn permission(&self) -> Result<Permission> {
        // Linux has no per-app notification permission in this protocol.
        Ok(Permission::Granted)
    }

    fn request_permission(&self) -> Result<Permission> {
        Ok(Permission::Granted)
    }

    fn min_progress_interval(&self) -> Duration {
        Duration::from_millis(250)
    }
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(error.to_string())
}

fn worker_gone() -> Error {
    Error::Backend("the D-Bus worker thread stopped".into())
}
