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
//! | Network up / down  | ✓ (NotifyIpInterfaceChange) | ✓ (polling / NWPathMonitor) |
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

pub mod common;
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
        let (tx, rx) = common::sync_channel(64);

        #[cfg(target_os = "windows")]
        {
            windows::start(tx);
            return Sentinel { rx };
        }

        #[cfg(target_os = "macos")]
        {
            let guard = macos::MacosGuard::new(tx);
            return Sentinel { rx, _guard: guard };
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
}
