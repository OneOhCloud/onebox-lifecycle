//! # onebox_lifecycle
//!
//! Cross-platform system lifecycle monitoring: shutdown blocking, sleep/wake,
//! and network up/down events.
//!
//! ---
//!
//! 跨平台系统生命周期监控：关机阻塞、睡眠/唤醒、网络上下线事件。
//!
//! ## Feature matrix / Feature 矩阵
//!
//! | Feature    | Windows | macOS |
//! |------------|---------|-------|
//! | `shutdown` | `WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate` | `applicationShouldTerminate:` → `NSTerminateLater` |
//! | `sleep`    | `WM_POWERBROADCAST` | `NSWorkspace` notifications |
//! | `network`  | `NotifyNetworkConnectivityHintChange` | `NWPathMonitor` (Network.framework) |
//!
//! All three features are enabled by default.
//! 三个 feature 默认全部启用。
//!
//! ## Quick start / 快速开始
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
//!                 // Do synchronous cleanup, then allow shutdown.
//!                 // 执行同步清理，然后允许关机。
//!                 handle.allow();
//!             }
//!             SystemEvent::WillSleep    => println!("Going to sleep / 即将睡眠"),
//!             SystemEvent::DidWake   => println!("Woke up / 已唤醒"),
//!             SystemEvent::NetworkUp   => println!("Network up / 网络可用"),
//!             SystemEvent::NetworkDown => println!("Network down / 网络不可用"),
//!             _ => {}
//!         }
//!     }
//! }
//! ```
//!
//! ## Async cleanup with Tokio / 使用 Tokio 进行异步清理
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
//! }
//! ```

pub(crate) mod common;
pub use common::{EventReceiver, EventSender, SystemEvent};
#[cfg(feature = "shutdown")]
pub use common::ShutdownHandle;
#[cfg(all(
    target_os = "windows",
    any(feature = "shutdown", feature = "sleep", feature = "network")
))]
mod windows;

#[cfg(all(
    target_os = "macos",
    any(feature = "shutdown", feature = "sleep", feature = "network")
))]
mod macos;

// ─── Sentinel ──────────────────────────────────────────────────────────────────

/// The main entry point. Call [`Sentinel::start`] once at application startup.
///
/// ---
///
/// 主入口点。在应用启动时调用一次 [`Sentinel::start`]。
pub struct Sentinel {
    rx: EventReceiver,
    /// Keeps macOS RAII resources (delegate, observers, monitors) alive.
    #[cfg(all(
        target_os = "macos",
        any(feature = "shutdown", feature = "sleep", feature = "network")
    ))]
    _guard: macos::MacosGuard,
}

impl Sentinel {
    /// Start the sentinel.
    ///
    /// **macOS**: installs an `NSApplicationDelegate` and registers power/network
    /// observers. **Must be called from the main thread.**
    ///
    /// **Windows**: spawns a background thread with a hidden Win32 window.
    ///
    /// ---
    ///
    /// 启动哨兵。
    ///
    /// **macOS**：安装 `NSApplicationDelegate` 并注册电源/网络观察者。**必须从主线程调用。**
    ///
    /// **Windows**：启动带隐藏 Win32 窗口的后台线程。
    pub fn start() -> Self {
        let (tx, rx) = common::channel();

        #[cfg(all(
            target_os = "windows",
            any(feature = "shutdown", feature = "sleep", feature = "network")
        ))]
        {
            windows::start(tx);
            return Sentinel { rx };
        }

        #[cfg(all(
            target_os = "macos",
            any(feature = "shutdown", feature = "sleep", feature = "network")
        ))]
        {
            let guard = macos::MacosGuard::new(tx);
            return Sentinel { rx, _guard: guard };
        }

        #[cfg(not(any(
            all(target_os = "windows", any(feature = "shutdown", feature = "sleep", feature = "network")),
            all(target_os = "macos",   any(feature = "shutdown", feature = "sleep", feature = "network")),
        )))]
        {
            drop(tx); // unsupported platform or no features enabled
            Sentinel { rx }
        }
    }

    /// Block until the next [`SystemEvent`] arrives.
    ///
    /// Returns `None` only if the sentinel has been shut down.
    ///
    /// ---
    ///
    /// 阻塞直到下一个 [`SystemEvent`] 到来；哨兵关闭时返回 `None`。
    pub fn recv(&self) -> Option<SystemEvent> {
        self.rx.recv()
    }

    /// Non-blocking poll.
    ///
    /// ---
    ///
    /// 非阻塞轮询。
    pub fn try_recv(&self) -> Option<SystemEvent> {
        self.rx.try_recv()
    }

    /// Consume the sentinel and return only the [`EventReceiver`].
    ///
    /// The receiver is `Send` and can be moved to any background thread.
    ///
    /// On **macOS** the platform guard (NSWorkspace observers, NWPathMonitor,
    /// NSApplicationDelegate) is intentionally leaked so it keeps running for
    /// the lifetime of the process. Use this when integrating with a framework
    /// (e.g. Tauri) that already runs its own `NSApplication` event loop on the
    /// main thread.
    ///
    /// ---
    ///
    /// 消耗哨兵并仅返回 [`EventReceiver`]（实现了 `Send`，可移至任意后台线程）。
    ///
    /// 在 **macOS** 上，平台守卫（NSWorkspace 观察者、NWPathMonitor、
    /// NSApplicationDelegate）会被有意泄漏，以确保其在进程生命周期内持续运行。
    /// 适用于与已在主线程运行 `NSApplication` 事件循环的框架（如 Tauri）集成。
    pub fn into_receiver(self) -> EventReceiver {
        #[cfg(all(
            target_os = "macos",
            any(feature = "shutdown", feature = "sleep", feature = "network")
        ))]
        {
            let (rx, guard) = (self.rx, self._guard);
            // NSApplication retains the delegate; NSNotificationCenter retains
            // the power observer; nw_path_monitor_start keeps the network monitor
            // alive — all resources continue working without the Rust wrapper.
            std::mem::forget(guard);
            return rx;
        }

        #[cfg(not(all(
            target_os = "macos",
            any(feature = "shutdown", feature = "sleep", feature = "network")
        )))]
        {
            self.rx
        }
    }
}
