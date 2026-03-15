# onebox-lifecycle

Cross-platform system lifecycle monitoring for Rust — shutdown blocking, sleep/wake, and network events.

## Feature matrix

| Feature | Windows | macOS |
|---------|---------|-------|
| Shutdown blocking | ✓ `WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate` | ✓ `NSTerminateLater` |
| Sleep / wake | ✓ `WM_POWERBROADCAST` | ✓ `NSWorkspace` notifications |
| Network up / down | ✓ `NotifyIpInterfaceChange` | ✓ `NWPathMonitor` |
| Async cleanup | ✓ handle-based, tokio-compatible | ✓ |

## Quick start

```rust
use onebox_lifecycle::{Sentinel, SystemEvent};

fn main() {
    // macOS: must be called from the main thread.
    let sentinel = Sentinel::start();

    while let Some(event) = sentinel.recv() {
        match event {
            SystemEvent::ShuttingDown(handle) => {
                // Do cleanup work, then allow the OS to proceed.
                handle.allow();
            }
            SystemEvent::WillSleep    => println!("going to sleep"),
            SystemEvent::DidWake      => println!("woke up"),
            SystemEvent::NetworkUp    => println!("network up"),
            SystemEvent::NetworkDown  => println!("network down"),
            _ => {}
        }
    }
}
```

## Async cleanup with Tokio

```rust
use onebox_lifecycle::{Sentinel, SystemEvent};

#[tokio::main]
async fn main() {
    let sentinel = Sentinel::start();

    while let Some(event) = sentinel.recv() {
        if let SystemEvent::ShuttingDown(handle) = event {
            tokio::spawn(async move {
                do_cleanup().await;
                handle.allow();
            });
        }
    }
}
```

## Tauri integration

Tauri already owns the `NSApplication` main thread, so calling `Sentinel::start()`
directly inside `setup` works — but `Sentinel` itself cannot be moved to a
background thread because its macOS guard is `MainThreadOnly`.

Use `into_receiver()` to split: the platform guard (NSApplicationDelegate,
NSWorkspace observer, NWPathMonitor) is leaked and stays alive for the process
lifetime, while the `EventReceiver` — which is `Send` — is forwarded to a
background thread.

**`src-tauri/Cargo.toml`**

```toml
[dependencies]
onebox_lifecycle = { path = "../onebox-lifecycle" }
# or once published:
# onebox_lifecycle = "0.1"
```

**`src-tauri/src/lib.rs`**

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleEvent {
    kind: &'static str,
    message: String,
    timestamp_ms: u64,
}

fn emit(handle: &AppHandle, kind: &'static str, message: impl Into<String>) {
    let _ = handle.emit("lifecycle-event", LifecycleEvent {
        kind,
        message: message.into(),
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap_or_default()
            .as_millis() as u64,
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();

            // Must be called from the main thread (Tauri's setup satisfies this).
            // into_receiver() leaks the macOS guard and returns a Send receiver.
            let rx = onebox_lifecycle::Sentinel::start().into_receiver();

            std::thread::Builder::new()
                .name("lifecycle-events".into())
                .spawn(move || {
                    while let Some(event) = rx.recv() {
                        use onebox_lifecycle::SystemEvent;
                        match event {
                            SystemEvent::WillSleep => {
                                emit(&handle, "WillSleep", "System going to sleep");
                            }
                            SystemEvent::DidWake => {
                                emit(&handle, "DidWake", "System resumed from sleep");
                            }
                            SystemEvent::NetworkUp => {
                                emit(&handle, "NetworkUp", "Network UP");
                            }
                            SystemEvent::NetworkDown => {
                                emit(&handle, "NetworkDown", "Network DOWN");
                            }
                            SystemEvent::ShuttingDown(shutdown_handle) => {
                                emit(&handle, "ShuttingDown", "Shutdown requested — cleaning up…");
                                let h = handle.clone();
                                std::thread::spawn(move || {
                                    // Do real cleanup here…
                                    std::thread::sleep(std::time::Duration::from_secs(3));
                                    emit(&h, "ShutdownComplete", "Done — allowing OS to proceed");
                                    shutdown_handle.allow();
                                });
                            }
                            _ => {}
                        }
                    }
                })
                .expect("failed to spawn lifecycle thread");

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**Frontend (JavaScript)**

```js
const { listen } = window.__TAURI__.event;

await listen('lifecycle-event', ({ payload }) => {
    const { kind, message, timestampMs } = payload;
    console.log(`[${new Date(timestampMs).toLocaleTimeString()}] ${kind}: ${message}`);
    // update your UI here
});
```

> **Why Tauri instead of a plain CLI for shutdown blocking?**
> A bare CLI process has no `.app` bundle. macOS does not send
> `applicationShouldTerminate:` to it — the process is simply `SIGKILL`ed during
> shutdown with no opportunity to block or log.  A Tauri app is a proper `.app`
> that macOS recognises, shows the "waiting for app…" spinner, and waits for
> `replyToApplicationShouldTerminate:` before proceeding.

## Running the demo

```bash
# Build and run (log written to ./onebox_lifecycle_demo.log)
make run

# Run detached from Terminal (required to test shutdown blocking)
make run-detached
make log      # tail the log in another terminal
make stop     # kill the background process

# Install as a launchd user agent (recommended for shutdown testing)
make install-agent
make uninstall-agent
```

## Why run detached from Terminal?

On macOS, shutdown asks Terminal to quit first. If Terminal kills its child
processes before they can call `replyToApplicationShouldTerminate:`, the OS
hangs indefinitely. Running detached (`make run-detached`) or as a launchd
agent (`make install-agent`) avoids this.

## Module structure

```
src/
├── lib.rs           Public API: Sentinel, SystemEvent
├── common/mod.rs    Shared types: ShutdownHandle, EventReceiver/Sender
├── macos/mod.rs     macOS backend: NSApplicationDelegate, NSWorkspace, NWPathMonitor
└── windows/mod.rs   Windows backend: hidden Win32 message-loop window

examples/
└── demo_full.rs     Full demo with file logging and async cleanup
```

---

[中文说明](README.zh.md)
