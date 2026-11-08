//! Walks through an AirDrop-style transfer: a request with Accept and Reject buttons,
//! then progress, then a completion alert.
//!
//!     cargo run --example demo            # Linux and Windows
//!     scripts/macos-demo.sh               # macOS, which needs an app bundle
//!
//! Pass `--probe` to report what the OS supports without prompting or showing anything
//! visible (a permission error is the expected result on a Mac that hasn't granted it).

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use zephyr_notify::{Action, Event, Notification, Notifier, Permission, Progress};

fn main() -> zephyr_notify::Result<()> {
    let (events_tx, events) = mpsc::channel();
    let notifier = Notifier::builder("com.zephyr.notify.demo")
        .app_name("Zephyr Notify Demo")
        .on_event(move |event| drop(events_tx.send(event)))
        .build()?;

    println!("capabilities: {:#?}", notifier.capabilities());
    println!("permission: {:?}", notifier.permission()?);

    let request = Notification::new("Alex wants to send you a file")
        .id("demo-transfer")
        .body("holiday.mov (1.2 GB)")
        .action(Action::button("accept", "Accept"))
        .action(Action::button("reject", "Reject").destructive())
        .action(Action::text_input("reply", "Reply").placeholder("Say something"))
        .persistent(true);

    if std::env::args().any(|arg| arg == "--probe") {
        let result = notifier.show(request).map(|handle| {
            drop(handle.close());
        });
        println!("show: {result:?}");
        return Ok(());
    }

    if notifier.request_permission()? == Permission::Denied {
        println!("permission denied; enable notifications for this app in system settings");
        return Ok(());
    }

    let transfer = notifier.show(request)?;
    println!("notification shown, waiting for a response...");

    let event = events.recv().expect("event thread stopped");
    println!("event: {event:?}");
    if !matches!(&event, Event::Action { action_id, .. } if action_id == "accept") {
        return Ok(());
    }

    // Pressing a button consumes the notification, so alert again to show progress.
    transfer.alert(|n| {
        n.title = "Receiving holiday.mov".into();
        n.actions = vec![Action::button("cancel", "Cancel").destructive()];
        n.progress = Some(Progress::fraction(0.0).status("From Alex"));
    })?;

    for percent in 1..=100_u32 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(event) = events.try_recv() {
            println!("event: {event:?}");
            println!("transfer stopped");
            return Ok(());
        }
        transfer.set_progress(
            Progress::fraction(f64::from(percent) / 100.0)
                .status("From Alex")
                .label(format!("{} MB of 1200 MB", percent * 12)),
        )?;
    }

    transfer.alert(|n| {
        n.title = "Transfer complete".into();
        n.body = Some("holiday.mov was saved to Downloads".into());
        n.progress = None;
        n.actions = vec![Action::button("open", "Open").foreground()];
    })?;

    println!("event: {:?}", events.recv_timeout(Duration::from_secs(30)));
    Ok(())
}
