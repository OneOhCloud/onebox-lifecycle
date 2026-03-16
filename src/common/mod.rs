/// A handle returned inside [`SystemEvent::ShuttingDown`].
///
/// The holder of this handle controls whether shutdown proceeds.
/// Call [`ShutdownHandle::block`] to delay shutdown (optionally with a reason),
/// then [`ShutdownHandle::allow`] once async cleanup has finished.
///
/// Dropping the handle without calling either method defaults to *allowing* shutdown.
#[cfg(feature = "shutdown")]
pub struct ShutdownHandle {
    /// Wrapped in `Option` so that `Drop` can take ownership without `unsafe`.
    pub(crate) inner: Option<ShutdownHandleInner>,
}

#[cfg(feature = "shutdown")]
pub(crate) enum ShutdownHandleInner {
    /// Resolved at construction time (e.g. macOS `replyToApplicationShouldTerminate:`).
    Mpsc(std::sync::mpsc::SyncSender<ShutdownDecision>),
    // Tokio variant kept behind feature flag so the crate is usable without tokio.
    #[cfg(feature = "tokio")]
    Tokio(tokio::sync::oneshot::Sender<ShutdownDecision>),
}

#[cfg(feature = "shutdown")]
#[derive(Debug)]
#[allow(dead_code)] // `reason` is read by the Windows backend
pub(crate) enum ShutdownDecision {
    Allow,
    Block { reason: Option<String> },
}

#[cfg(feature = "shutdown")]
impl ShutdownHandle {
    /// Tell the OS: "I'm not ready yet — please wait."
    ///
    /// `reason` is displayed to the user on Windows (max ~512 chars).
    /// On macOS the string is ignored (the system controls the UI).
    pub fn block(mut self, reason: impl Into<String>) {
        let reason = Some(reason.into());
        self.send_inner(ShutdownDecision::Block { reason });
    }

    /// Tell the OS: "I'm done — go ahead and shut down."
    pub fn allow(mut self) {
        self.send_inner(ShutdownDecision::Allow);
    }

    fn send_inner(&mut self, decision: ShutdownDecision) {
        if let Some(inner) = self.inner.take() {
            match inner {
                ShutdownHandleInner::Mpsc(tx) => {
                    let _ = tx.send(decision);
                }
                #[cfg(feature = "tokio")]
                ShutdownHandleInner::Tokio(tx) => {
                    let _ = tx.send(decision);
                }
            }
        }
    }
}

#[cfg(feature = "shutdown")]
impl Drop for ShutdownHandle {
    /// If the caller drops the handle without deciding, default to `Allow`.
    fn drop(&mut self) {
        self.send_inner(ShutdownDecision::Allow);
    }
}

// ─── Events ──────────────────────────────────────────────────────────────────

/// Events emitted by the sentinel to application code.
#[non_exhaustive]
pub enum SystemEvent {
    /// The system is about to suspend. Return quickly; you have very little time.
    #[cfg(feature = "sleep")]
    WillSleep,
    /// The system has resumed from suspend.
    #[cfg(feature = "sleep")]
    DidWake,
    /// A network interface has come up (at least one route is reachable).
    #[cfg(feature = "network")]
    NetworkUp,
    /// All network interfaces are gone / unreachable.
    #[cfg(feature = "network")]
    NetworkDown,
    /// The user (or system policy) has requested shutdown/logout/restart.
    ///
    /// Use [`ShutdownHandle::block`] to delay it while you do async cleanup,
    /// then [`ShutdownHandle::allow`] when ready.
    #[cfg(feature = "shutdown")]
    ShuttingDown(ShutdownHandle),
}

impl std::fmt::Debug for SystemEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "sleep")]
            SystemEvent::WillSleep => write!(f, "WillSleep"),
            #[cfg(feature = "sleep")]
            SystemEvent::DidWake => write!(f, "DidWake"),
            #[cfg(feature = "network")]
            SystemEvent::NetworkUp => write!(f, "NetworkUp"),
            #[cfg(feature = "network")]
            SystemEvent::NetworkDown => write!(f, "NetworkDown"),
            #[cfg(feature = "shutdown")]
            SystemEvent::ShuttingDown(_) => write!(f, "ShuttingDown(<handle>)"),
            // When some features are disabled, the enum may have no visible variants.
            // The wildcard arm keeps the match exhaustive in all configurations.
            #[allow(unreachable_patterns)]
            _ => write!(f, "SystemEvent(<unknown>)"),
        }
    }
}

// ─── Channel helpers ─────────────────────────────────────────────────────────

/// Create an unbounded event channel.
///
/// The sender never blocks, which is critical for platform backends that call
/// `send` from inside a Win32 window procedure or an OS callback.
pub fn channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = std::sync::mpsc::channel();
    (EventSender { tx }, EventReceiver { rx })
}

pub struct EventSender {
    pub(crate) tx: std::sync::mpsc::Sender<SystemEvent>,
}

pub struct EventReceiver {
    rx: std::sync::mpsc::Receiver<SystemEvent>,
}

impl EventSender {
    pub(crate) fn send(&self, ev: SystemEvent) {
        // Ignore send errors — the receiver may have been dropped intentionally.
        let _ = self.tx.send(ev);
    }
}

impl EventReceiver {
    /// Block until the next event arrives.
    pub fn recv(&self) -> Option<SystemEvent> {
        self.rx.recv().ok()
    }

    /// Non-blocking poll.
    pub fn try_recv(&self) -> Option<SystemEvent> {
        self.rx.try_recv().ok()
    }
}

#[cfg(feature = "tokio")]
pub mod tokio_support {
    use super::SystemEvent;

    /// Async-compatible event channel backed by `tokio::sync::mpsc`.
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
            // `tokio::spawn` panics when called outside a Tokio runtime (e.g. from
            // the Win32 message-loop thread).  Use `try_current` so we only spawn
            // a task when a runtime is actually present; otherwise fall back to
            // `try_send` (lifecycle events are rare enough that backpressure drop
            // is acceptable as a last resort).
            let tx = self.tx.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let _ = tx.send(ev).await;
                    });
                }
                Err(_) => {
                    let _ = self.tx.try_send(ev);
                }
            }
        }
    }

    impl AsyncEventReceiver {
        pub async fn recv(&mut self) -> Option<SystemEvent> {
            self.rx.recv().await
        }
    }
}
