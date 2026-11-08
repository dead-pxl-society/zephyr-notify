# zephyr-notify

Native desktop notifications with action buttons and progress, behind one Rust API for
macOS, Windows and Linux. No async runtime required, so it works alongside Tauri's.

```rust
use zephyr_notify::{Action, Event, Notification, Notifier, Progress};

let notifier = Notifier::builder("com.example.zephyr")
    .app_name("Zephyr")
    .on_event(|event| match event {
        Event::Action { id, action_id, .. } => println!("{id}: {action_id}"),
        other => println!("{other:?}"),
    })
    .build()?;
notifier.request_permission()?; // Blocks on macOS until the user answers.

let transfer = notifier.show(
    Notification::new("Alex wants to send you a file")
        .id("transfer-42")
        .body("holiday.mov (1.2 GB)")
        .action(Action::button("accept", "Accept"))
        .action(Action::button("reject", "Reject").destructive().secondary())
        .persistent(true),
)?;

// A button press consumes the notification, so `alert` to show a new one.
transfer.alert(|n| {
    n.title = "Receiving holiday.mov".into();
    n.actions.clear();
    n.progress = Some(Progress::fraction(0.0));
})?;
transfer.set_progress(Progress::fraction(0.25).label("12 MB/s"))?; // Fine in a tight loop.
```

## What each platform does

| Feature | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Action buttons | Yes; more than one goes in an "Options" menu | Yes | Yes, if the server has the `actions` capability |
| Secondary actions | Left out | Shown | Shown |
| Inline text reply | Yes | Yes | Yes, if the server has `inline-reply`; otherwise a plain button |
| Progress | Text in the body | Native bar | `value` hint, and text on servers not known to draw it |
| Quiet updates | Passive replacement | In-place data binding | Replace with sound suppressed |
| Image | Attachment | App logo override | `image-path` hint |
| Keep on screen | User's alert style decides | `reminder` scenario | No expiry (the server may ignore it) |
| Button press after the app quit | Launches the app | Launches the app | Not possible |

`Notifier::capabilities()` reports this at runtime.

## Features

- Accept/reject style buttons, destructive styling, and optional focus on press
- Secondary actions, so the main action isn't buried in a menu on macOS
- Text input on a notification
- Determinate and indeterminate progress, with status and label text, rate limited for you
- Quiet updates that don't re-alert, and don't resurrect a notification the user dismissed
- Events for button presses, body clicks and dismissals
- Thumbnails, sounds, urgency, grouping, and closing by id
- Permission checks and requests
- Escaping of text from peers, so a hostile file name can't inject toast XML or markup

## Using it with Tauri

Build the notifier **in `main`, before `tauri::Builder`**. On macOS and Windows the press
that launched your app is delivered as soon as `build` runs, and on macOS the delegate has
to be set before launch finishes. Your handler runs before there's an `AppHandle`, so
queue events until `setup`:

```rust
fn main() {
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let notifier = Notifier::builder("com.example.zephyr") // Your Tauri `identifier`.
        .app_name("Zephyr")
        .icon(icon_path()) // Windows and Linux.
        .on_event(move |event| drop(events_tx.send(event)))
        .build();

    tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                for event in events_rx {
                    // Forward to your transfer logic.
                }
            });
            match notifier {
                Ok(notifier) => { app.manage(notifier); }
                // `NotBundled` is expected under `tauri dev` on macOS.
                Err(error) => log::warn!("notifications unavailable: {error}"),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap();
}
```

`request_permission` blocks until the user answers on macOS, so call it from
`tauri::async_runtime::spawn_blocking`. Every other method is a short round trip to the OS.

## Platform setup

### macOS

- **The app must run from a signed `.app` bundle.** Otherwise `build` returns
  `Error::NotBundled`, which includes `cargo run` and `tauri dev`. Test with a real bundle,
  such as `tauri build --debug`, or see `scripts/macos-demo.sh`.
  An ad-hoc signature isn't enough: macOS reports permission denied without prompting.
- **More than one action goes behind an "Options" menu.** Mark the actions users can also
  take in your app with `Action::secondary()`, and they're left out here so the rest stay
  on the notification.
- **Buttons are hidden by default.** With the default banner style they appear on hover,
  and the banner goes away after a few seconds. Users can choose the Alerts style in System
  Settings, and only that keeps a notification on screen until it's answered.
- **There's no progress bar API.** Progress is a line of text such as
  `▓▓▓▓░░░░░░ 40% · 12 MB/s`, updated once a second.
- `Urgency::High` needs the time-sensitive notifications entitlement to have any effect.
- Don't put per-transfer values in button labels. Each distinct set of buttons is stored
  by the OS as a category, and categories accumulate.

### Windows

- `build` registers the app under `HKCU\Software\Classes` (`AppUserModelId\<app id>` and
  `CLSID\<derived>`), so it needs no admin rights and no installer change. Call
  `zephyr_notify::unregister_windows_app` from your uninstaller to remove the entries.
- **A press after the app has quit launches your exe with `-Embedding`** as an argument.
  Check for it if you'd rather start hidden, for example when the press was "Reject".
- Toast images must be local files of at most about 3 MB.

### Linux

- Needs a D-Bus session bus and a notification server; `build` returns an error otherwise.
- Actions only work while your process runs, because the protocol has no way to relaunch it.
- The body click action is labeled "Open", which some servers show.
- Under Flatpak, add `--talk-name=org.freedesktop.Notifications` to the manifest.

## Development

```sh
cargo test                       # Unit tests on any OS.
scripts/macos-demo.sh            # Full flow on macOS.
cargo run --example demo         # Full flow on Windows or Linux.

# Linux backend against a mock server over real D-Bus.
dbus-run-session -- cargo test --test linux_dbus -- --test-threads=1
```
