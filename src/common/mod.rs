//! Shared event types and channel helpers used by all platform backends.
//! 所有平台后端共用的事件类型与通道工具。

// ─── Feature submodules ────────────────────────────────────────────────────────

#[cfg(feature = "shutdown")]
pub(crate) mod shutdown;

// ─── Public re-exports from feature submodules ─────────────────────────────────

#[cfg(feature = "shutdown")]
pub use shutdown::ShutdownHandle;

// ─── SystemEvent ───────────────────────────────────────────────────────────────

/// Events emitted by the sentinel to application code.
///
/// The available variants depend on which features are enabled.
///
/// ---
///
/// 哨兵向应用代码发出的事件，可用变体取决于启用的 feature。
#[non_exhaustive]
pub enum SystemEvent {
    /// The system is about to enter sleep. Cannot be delayed.
    ///
    /// **macOS**: `NSWorkspaceWillSleepNotification`.
    /// **Windows**: `WM_POWERBROADCAST` / `PBT_APMSUSPEND`.
    ///
    /// ---
    ///
    /// 系统即将进入睡眠，无法延迟。
    ///
    /// **macOS**：`NSWorkspaceWillSleepNotification`。
    /// **Windows**：`WM_POWERBROADCAST` / `PBT_APMSUSPEND`。
    #[cfg(feature = "sleep")]
    WillSleep,

    /// The system has resumed from suspend.
    ///
    /// **macOS**: `NSWorkspaceDidWakeNotification`.
    /// **Windows**: `WM_POWERBROADCAST` / `PBT_APMRESUMESUSPEND`.
    ///
    /// ---
    ///
    /// 系统已从挂起中恢复。
    ///
    /// **macOS**：`NSWorkspaceDidWakeNotification`。
    /// **Windows**：`WM_POWERBROADCAST` / `PBT_APMRESUMESUSPEND`。
    #[cfg(feature = "sleep")]
    DidWake,

    /// At least one network interface is reachable.
    ///
    /// **macOS**: `NWPathMonitor` path status `satisfied` (Network.framework, 10.14+).
    /// **Windows**: `NotifyNetworkConnectivityHintChange` / `InternetAccess` or
    /// `ConstrainedInternetAccess`.
    ///
    /// ---
    ///
    /// 至少一个网络接口可达。
    ///
    /// **macOS**：`NWPathMonitor` 路径状态 `satisfied`（Network.framework，10.14+）。
    /// **Windows**：`NotifyNetworkConnectivityHintChange` / `InternetAccess` 或
    /// `ConstrainedInternetAccess`。
    #[cfg(feature = "network")]
    NetworkUp,

    /// All network interfaces are gone or unreachable.
    ///
    /// ---
    ///
    /// 所有网络接口均不可达。
    #[cfg(feature = "network")]
    NetworkDown,

    /// The user or system policy has requested shutdown, logout, or restart.
    ///
    /// Use [`ShutdownHandle::block`] to delay it while doing async cleanup,
    /// then [`ShutdownHandle::allow`] when ready.
    ///
    /// **macOS**: `applicationShouldTerminate:` returning `NSTerminateLater`.
    /// **Windows**: `WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate`.
    ///
    /// ---
    ///
    /// 用户或系统策略请求关机、注销或重启。
    ///
    /// 调用 [`ShutdownHandle::block`] 延迟，清理完成后调用 [`ShutdownHandle::allow`]。
    ///
    /// **macOS**：`applicationShouldTerminate:` 返回 `NSTerminateLater`。
    /// **Windows**：`WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate`。
    #[cfg(feature = "shutdown")]
    ShuttingDown(shutdown::ShutdownHandle),

}

impl std::fmt::Debug for SystemEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "sleep")]
            SystemEvent::WillSleep       => write!(f, "WillSleep"),
            #[cfg(feature = "sleep")]
            SystemEvent::DidWake         => write!(f, "DidWake"),
            #[cfg(feature = "network")]
            SystemEvent::NetworkUp       => write!(f, "NetworkUp"),
            #[cfg(feature = "network")]
            SystemEvent::NetworkDown     => write!(f, "NetworkDown"),
            #[cfg(feature = "shutdown")]
            SystemEvent::ShuttingDown(_) => write!(f, "ShuttingDown(<handle>)"),
            #[allow(unreachable_patterns)]
            _ => write!(f, "SystemEvent(<unknown>)"),
        }
    }
}

// ─── Channel helpers ───────────────────────────────────────────────────────────

/// Create an unbounded synchronous event channel.
///
/// The sender never blocks, which is required for platform backends that call
/// `send` from a Win32 window procedure or an OS power callback.
///
/// ---
///
/// 创建无界同步事件通道。
///
/// 发送端永不阻塞，这是从 Win32 窗口过程或 OS 电源回调中发送事件的前提。
pub fn channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = std::sync::mpsc::channel();
    (EventSender { tx }, EventReceiver { rx })
}

/// Sending half of the event channel. Cheap to clone via [`Arc`] at the call site.
///
/// ---
///
/// 事件通道的发送端。调用端可通过 [`Arc`] 廉价共享。
pub struct EventSender {
    pub(crate) tx: std::sync::mpsc::Sender<SystemEvent>,
}

/// Receiving half of the event channel. `Send`, so it can move to any thread.
///
/// ---
///
/// 事件通道的接收端，实现了 `Send`，可移至任意线程。
pub struct EventReceiver {
    rx: std::sync::mpsc::Receiver<SystemEvent>,
}

impl EventSender {
    pub(crate) fn send(&self, ev: SystemEvent) {
        let _ = self.tx.send(ev);
    }
}

impl EventReceiver {
    /// Block until the next event arrives. Returns `None` if the sender is dropped.
    ///
    /// ---
    ///
    /// 阻塞直到下一个事件到来；发送端丢弃时返回 `None`。
    pub fn recv(&self) -> Option<SystemEvent> {
        self.rx.recv().ok()
    }

    /// Non-blocking poll. Returns `None` if no event is ready.
    ///
    /// ---
    ///
    /// 非阻塞轮询；无事件时返回 `None`。
    pub fn try_recv(&self) -> Option<SystemEvent> {
        self.rx.try_recv().ok()
    }
}

// ─── Tokio async channel ───────────────────────────────────────────────────────

/// Async-compatible channel backed by `tokio::sync::mpsc`.
///
/// ---
///
/// 基于 `tokio::sync::mpsc` 的异步兼容通道。
#[cfg(feature = "tokio")]
pub mod tokio_support {
    use super::SystemEvent;

    /// Create an async event channel with the given buffer capacity.
    ///
    /// ---
    ///
    /// 创建指定缓冲容量的异步事件通道。
    pub fn channel(cap: usize) -> (AsyncEventSender, AsyncEventReceiver) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        (AsyncEventSender { tx }, AsyncEventReceiver { rx })
    }

    pub struct AsyncEventSender {
        pub(crate) tx: tokio::sync::mpsc::Sender<SystemEvent>,
    }

    pub struct AsyncEventReceiver {
        rx: tokio::sync::mpsc::Receiver<SystemEvent>,
    }

    impl AsyncEventSender {
        pub(crate) fn send(&self, ev: SystemEvent) {
            // `tokio::spawn` panics outside a runtime. Use `try_current` to
            // detect the runtime; fall back to `try_send` when absent.
            //
            // `tokio::spawn` 在运行时外会 panic。用 `try_current` 检测运行时；
            // 不存在时回退到 `try_send`（生命周期事件频率低，偶发丢弃可接受）。
            let tx = self.tx.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => { handle.spawn(async move { let _ = tx.send(ev).await; }); }
                Err(_)     => { let _ = self.tx.try_send(ev); }
            }
        }
    }

    impl AsyncEventReceiver {
        pub async fn recv(&mut self) -> Option<SystemEvent> {
            self.rx.recv().await
        }
    }
}
