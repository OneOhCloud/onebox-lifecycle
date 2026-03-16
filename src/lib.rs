//! # onebox_lifecycle
//!
//! Cross-platform system lifecycle monitoring for Rust.
//!
//! ## Features
//!
//! | Feature            | Windows | macOS |
//! |--------------------|---------|-------|
//! | Shutdown blocking  | ✓ (WM_QUERYENDSESSION + ShutdownBlockReasonCreate) | ✓ (NSTerminateLater) |
//! | Sleep / wake       | ✓ (WM_POWERBROADCAST) | ✓ (NSWorkspace notifications) |
//! | Network up / down  | ✓ (NotifyNetworkConnectivityHintChange) | ✓ (NWPathMonitor) |
//! | Async cleanup      | ✓ (handle-based, tokio-compatible) | ✓ |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use onebox_lifecycle::{Sentinel, SystemEvent};
//!
//! fn main() {
//!     let sentinel = Sentinel::start();
//!
//!     while let Some(event) = sentinel.recv() {
//!         match event {
//!             SystemEvent::ShuttingDown(handle) => {
//!                 println!("Shutdown requested — cleaning up…");
//!                 // Do synchronous work here, then:
//!                 handle.allow();
//!             }
//!             SystemEvent::WillSleep => println!("Going to sleep"),
//!             SystemEvent::DidWake   => println!("Woke up"),
//!             SystemEvent::NetworkUp   => println!("Network up"),
//!             SystemEvent::NetworkDown => println!("Network down"),
//!         }
//!     }
//! }
//! ```
//!
//! ## Async cleanup with Tokio
//!
//! ```rust,no_run
//! use onebox_lifecycle::{Sentinel, SystemEvent};
//!
//! #[tokio::main]
//! async fn main() {
//!     let sentinel = Sentinel::start();
//!
//!     while let Some(event) = sentinel.recv() {
//!         if let SystemEvent::ShuttingDown(handle) = event {
//!             // Spawn cleanup on the tokio runtime.
//!             tokio::spawn(async move {
//!                 do_async_cleanup().await;
//!                 handle.allow();
//!             });
//!         }
//!     }
//! }
//!
//! async fn do_async_cleanup() {
//!     tokio::time::sleep(std::time::Duration::from_secs(3)).await;
//!     println!("Cleanup done.");
//! }
//! ```

pub(crate) mod common;
pub use common::{EventReceiver, EventSender, ShutdownHandle, SystemEvent};

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

// ─── Platform-agnostic facade ─────────────────────────────────────────────────

/// The main entry-point.  Call [`Sentinel::start`] once at application startup.
pub struct Sentinel {
    rx: EventReceiver,
    /// Keeps platform-specific resources alive.
    #[cfg(target_os = "macos")]
    _guard: macos::MacosGuard,
}

impl Sentinel {
    /// Start the sentinel.
    ///
    /// On **Windows**: spawns a background thread with a hidden Win32 window.
    ///
    /// On **macOS**: installs an `NSApplicationDelegate` and registers for
    /// `NSWorkspace` power notifications.  **Must be called from the main thread.**
    pub fn start() -> Self {
        let (tx, rx) = common::channel();

        #[cfg(target_os = "windows")]
        {
            windows::start(tx);
            Sentinel { rx }
        }

        #[cfg(target_os = "macos")]
        {
            let guard = macos::MacosGuard::new(tx);
            Sentinel { rx, _guard: guard }
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            drop(tx); // unsupported platform — channel will immediately return None
            Sentinel { rx }
        }
    }

    /// Block until the next [`SystemEvent`] arrives.
    ///
    /// Returns `None` only if the sentinel has been shut down.
    pub fn recv(&self) -> Option<SystemEvent> {
        self.rx.recv()
    }

    /// Non-blocking poll.
    pub fn try_recv(&self) -> Option<SystemEvent> {
        self.rx.try_recv()
    }

    /// Consume the sentinel and return only the [`EventReceiver`], which is
    /// `Send` and can be moved to any background thread.
    ///
    /// On **macOS** the platform guard (NSWorkspace observers + NWPathMonitor +
    /// NSApplicationDelegate) is intentionally leaked so that it keeps running
    /// for the lifetime of the process.  Use this when integrating with a
    /// framework (e.g. Tauri) that already runs its own `NSApplication` event
    /// loop on the main thread.
    pub fn into_receiver(self) -> EventReceiver {
        #[cfg(target_os = "macos")]
        {
            let (rx, guard) = (self.rx, self._guard);
            // Forget the guard: NSApplication retains the delegate, NSNotificationCenter
            // retains the power observer, and nw_path_monitor_start keeps the network
            // monitor alive — all resources keep working without the Rust wrapper.
            std::mem::forget(guard);
            rx
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.rx
        }
    }
}
