//! Runs the Linux backend over real D-Bus against a mock notification server.
//!
//!     dbus-run-session -- cargo test --test linux_dbus -- --test-threads=1
//!
//! Tests share one session bus, and only one process can own the server's name at a time.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::interface;
use zbus::zvariant::OwnedValue;
use zephyr_notify::{
    Action, DismissReason, Event, Notification, Notifier, Progress, ProgressSupport,
};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

#[derive(Debug, Clone)]
struct Call {
    app_name: String,
    replaces_id: u32,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire_timeout: i32,
}

#[derive(Default)]
struct Log {
    notify: Vec<Call>,
    closed: Vec<u32>,
}

struct MockServer {
    log: Arc<Mutex<Log>>,
    next_id: u32,
    server_name: String,
    capabilities: Vec<String>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl MockServer {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        self.log.lock().unwrap().notify.push(Call {
            app_name,
            replaces_id,
            summary,
            body,
            actions,
            hints,
            expire_timeout,
        });
        if replaces_id != 0 {
            return replaces_id;
        }
        self.next_id += 1;
        self.next_id
    }

    fn close_notification(&self, id: u32) {
        self.log.lock().unwrap().closed.push(id);
    }

    fn get_capabilities(&self) -> Vec<String> {
        self.capabilities.clone()
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            self.server_name.clone(),
            "test".into(),
            "1".into(),
            "1.2".into(),
        )
    }
}

struct Fixture {
    server: Connection,
    log: Arc<Mutex<Log>>,
    notifier: Notifier,
    events: Receiver<Event>,
}

impl Fixture {
    fn new(server_name: &str, capabilities: &[&str]) -> Self {
        let log = Arc::new(Mutex::new(Log::default()));
        let mock = MockServer {
            log: log.clone(),
            next_id: 100,
            server_name: server_name.to_owned(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        };
        let server = Builder::session()
            .unwrap()
            .name(NAME)
            .unwrap()
            .serve_at(PATH, mock)
            .unwrap()
            .build()
            .unwrap();
        let (tx, events) = mpsc::channel();
        let notifier = Notifier::builder("com.example.test")
            .app_name("Test")
            .on_event(move |event| drop(tx.send(event)))
            .build()
            .unwrap();
        Self {
            server,
            log,
            notifier,
            events,
        }
    }

    fn signal<B>(&self, member: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.server
            .emit_signal(None::<&str>, PATH, NAME, member, body)
            .unwrap();
    }

    fn next_event(&self) -> Event {
        self.events
            .recv_timeout(Duration::from_secs(5))
            .expect("no event arrived")
    }

    fn calls(&self) -> Vec<Call> {
        self.log.lock().unwrap().notify.clone()
    }

    fn wait_for_close(&self, id: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.log.lock().unwrap().closed.contains(&id) {
            assert!(Instant::now() < deadline, "server never got close for {id}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Free the name for the next test.
        let _ = self.server.release_name(NAME);
    }
}

fn request() -> Notification {
    Notification::new("Alex wants to send a file")
        .id("transfer-1")
        .body("a.png")
        .action(Action::button("accept", "Accept"))
        .action(Action::button("reject", "Reject"))
}

#[test]
fn sends_actions_to_the_server_and_reports_the_press() {
    let f = Fixture::new("mock", &["actions", "body"]);
    assert!(f.notifier.capabilities().actions);
    let handle = f.notifier.show(request()).unwrap();

    let call = &f.calls()[0];
    assert_eq!(call.app_name, "Test");
    assert_eq!(call.summary, "Alex wants to send a file");
    assert_eq!(call.body, "a.png");
    assert_eq!(call.replaces_id, 0);
    assert_eq!(call.expire_timeout, -1);
    assert_eq!(
        call.actions,
        ["default", "Open", "accept", "Accept", "reject", "Reject"]
    );
    assert_eq!(
        call.hints["desktop-entry"],
        OwnedValue::try_from(zbus::zvariant::Value::from("com.example.test")).unwrap()
    );
    assert!(handle.is_active());

    f.signal("ActionInvoked", &(101_u32, "accept"));
    assert_eq!(
        f.next_event(),
        Event::Action {
            id: "transfer-1".into(),
            action_id: "accept".into(),
            input: None
        }
    );
    // Closed so that every server matches macOS and Windows, where a press consumes it.
    f.wait_for_close(101);
    assert!(!handle.is_active());
}

#[test]
fn signals_for_other_apps_are_ignored() {
    let f = Fixture::new("mock", &["actions"]);
    f.notifier.show(request()).unwrap();
    f.signal("ActionInvoked", &(999_u32, "accept"));
    f.signal("NotificationClosed", &(999_u32, 2_u32));
    f.signal("ActionInvoked", &(101_u32, "default"));
    assert_eq!(
        f.next_event(),
        Event::Activated {
            id: "transfer-1".into()
        }
    );
}

#[test]
fn progress_updates_replace_and_stop_after_dismissal() {
    let f = Fixture::new("gnome-shell", &["actions"]);
    let handle = f
        .notifier
        .show(
            Notification::new("Receiving")
                .id("p")
                .progress(Progress::fraction(0.1)),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    handle
        .set_progress(Progress::fraction(0.5).label("12 MB/s"))
        .unwrap();

    let calls = f.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].replaces_id, 101);
    assert_eq!(calls[1].hints["value"], OwnedValue::from(50_i32));
    assert_eq!(calls[1].hints["suppress-sound"], OwnedValue::from(true));
    assert_eq!(calls[1].body, "▓▓▓▓▓░░░░░ 50% · 12 MB/s");
    assert_eq!(calls[1].expire_timeout, 0);

    f.signal("NotificationClosed", &(101_u32, 2_u32));
    assert_eq!(
        f.next_event(),
        Event::Dismissed {
            id: "p".into(),
            reason: DismissReason::User
        }
    );
    handle.set_progress(Progress::fraction(1.0)).unwrap();
    handle.update(|n| n.title = "changed".into()).unwrap();
    assert_eq!(
        f.calls().len(),
        2,
        "a dismissed notification must not come back"
    );

    // But an explicit alert does bring it back, as a new notification.
    handle.alert(|n| n.title = "Done".into()).unwrap();
    let calls = f.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].replaces_id, 0);
    assert_eq!(calls[2].summary, "Done");
}

#[test]
fn tight_progress_loop_is_rate_limited_but_completion_gets_through() {
    let f = Fixture::new("dunst", &["actions"]);
    assert_eq!(f.notifier.capabilities().progress, ProgressSupport::Native);
    let handle = f
        .notifier
        .show(
            Notification::new("r")
                .id("p")
                .progress(Progress::fraction(0.0)),
        )
        .unwrap();
    for i in 0..10_000 {
        handle
            .set_progress(Progress::fraction(f64::from(i) / 10_001.0))
            .unwrap();
    }
    handle.set_progress(Progress::fraction(1.0)).unwrap();
    let calls = f.calls();
    assert!(calls.len() < 20, "sent {} updates", calls.len());
    assert_eq!(
        calls.last().unwrap().hints["value"],
        OwnedValue::from(100_i32)
    );
}

#[test]
fn inline_reply_delivers_text() {
    let f = Fixture::new("plasma", &["actions", "inline-reply"]);
    assert!(f.notifier.capabilities().text_input);
    f.notifier
        .show(
            Notification::new("m")
                .id("r")
                .action(Action::text_input("reply", "Reply")),
        )
        .unwrap();
    assert_eq!(f.calls()[0].actions[2], "inline-reply");
    f.signal("NotificationReplied", &(101_u32, "hello"));
    assert_eq!(
        f.next_event(),
        Event::Action {
            id: "r".into(),
            action_id: "reply".into(),
            input: Some("hello".into())
        }
    );
}

#[test]
fn close_tells_the_server_and_produces_no_event() {
    let f = Fixture::new("mock", &["actions"]);
    let handle = f.notifier.show(request()).unwrap();
    handle.close().unwrap();
    f.wait_for_close(101);
    assert!(!handle.is_active());
    // The server's own echo of our close, which must not look like a user dismissal.
    f.signal("NotificationClosed", &(101_u32, 3_u32));
    assert!(f.events.recv_timeout(Duration::from_millis(200)).is_err());
}

#[test]
fn updates_work_from_other_threads() {
    let f = Fixture::new("mock", &["actions"]);
    let handle = f.notifier.show(request()).unwrap();
    let clone = handle.clone();
    std::thread::spawn(move || clone.update(|n| n.title = "from thread".into()).unwrap())
        .join()
        .unwrap();
    assert_eq!(f.calls()[1].summary, "from thread");
    assert_eq!(f.calls()[1].replaces_id, 101);
}

#[test]
fn concurrent_is_active_and_updates_do_not_deadlock() {
    let f = Fixture::new("mock", &["actions"]);
    let handle = f.notifier.show(request()).unwrap();
    let updater = handle.clone();
    let worker = std::thread::spawn(move || {
        for i in 0..2000 {
            updater.update(|n| n.title = format!("{i}")).unwrap();
        }
    });
    while !worker.is_finished() {
        assert!(handle.is_active());
    }
    worker.join().unwrap();
}

#[test]
fn no_server_is_a_clean_error() {
    // Nothing owns the name here, so the backend must return an error, not hang.
    let result = Notifier::builder("com.example.test").build();
    assert!(result.is_err());
}
