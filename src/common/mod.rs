/// A handle returned inside [`SystemEvent::ShuttingDown`].
///
/// The holder of this handle controls whether shutdown proceeds.
/// Call [`ShutdownHandle::block`] to delay shutdown (optionally with a reason),
/// then [`ShutdownHandle::allow`] once async cleanup has finished.
///
/// Dropping the handle without calling either method defaults to *allowing* shutdown.
pub struct ShutdownHandle {
    /// Wrapped in `Option` so that `Drop` can take ownership without `unsafe`.
    pub(crate) inner: Option<ShutdownHandleInner>,
}

pub(crate) enum ShutdownHandleInner {
    /// Resolved at construction time (e.g. macOS `replyToApplicationShouldTerminate:`).
    Mpsc(std::sync::mpsc::SyncSender<ShutdownDecision>),
    // Tokio variant kept behind feature flag so the crate is usable without tokio.
    #[cfg(feature = "tokio")]
    Tokio(tokio::sync::oneshot::Sender<ShutdownDecision>),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // `reason` is read by the Windows backend
pub(crate) enum ShutdownDecision {
    Allow,
    Block { reason: Option<String> },
}

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
    WillSleep,
    /// The system has resumed from suspend.
    DidWake,
    /// A network interface has come up (at least one route is reachable).
    NetworkUp,
    /// All network interfaces are gone / unreachable.
    NetworkDown,
    /// The user (or system policy) has requested shutdown/logout/restart.
    ///
    /// Use [`ShutdownHandle::block`] to delay it while you do async cleanup,
    /// then [`ShutdownHandle::allow`] when ready.
    ShuttingDown(ShutdownHandle),
}

impl std::fmt::Debug for SystemEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SystemEvent::WillSleep => write!(f, "WillSleep"),
            SystemEvent::DidWake => write!(f, "DidWake"),
            SystemEvent::NetworkUp => write!(f, "NetworkUp"),
            SystemEvent::NetworkDown => write!(f, "NetworkDown"),
            SystemEvent::ShuttingDown(_) => write!(f, "ShuttingDown(<handle>)"),
        }
    }
}

// ─── Channel helpers ─────────────────────────────────────────────────────────

/// Synchronous multi-producer, single-consumer event channel.
pub fn sync_channel(cap: usize) -> (EventSender, EventReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel(cap);
    (EventSender { tx }, EventReceiver { rx })
}

pub struct EventSender {
    pub(crate) tx: std::sync::mpsc::SyncSender<SystemEvent>,
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
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(ev).await;
            });
        }
    }

    impl AsyncEventReceiver {
        pub async fn recv(&mut self) -> Option<SystemEvent> {
            self.rx.recv().await
        }
    }
}
