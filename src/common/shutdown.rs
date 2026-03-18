//! Shutdown handle types — compiled only when the `shutdown` feature is enabled.
//! 关机句柄类型——仅在启用 `shutdown` feature 时编译。

// ─── Public handle ─────────────────────────────────────────────────────────────

/// Controls whether the OS shutdown/logout/restart proceeds.
///
/// Returned inside [`SystemEvent::ShuttingDown`]. Call [`block`](Self::block)
/// to delay shutdown, then [`allow`](Self::allow) once cleanup is done.
/// Dropping without calling either defaults to allowing shutdown.
///
/// ---
///
/// 控制操作系统关机 / 注销 / 重启是否继续。
///
/// 随 [`SystemEvent::ShuttingDown`] 一同返回。调用 [`block`](Self::block) 延迟关机，
/// 清理完成后调用 [`allow`](Self::allow)。不调用任何方法直接丢弃，等同于调用 `allow`。
pub struct ShutdownHandle {
    /// Wrapped in `Option` so `Drop` can take ownership without `unsafe`.
    pub(crate) inner: Option<ShutdownHandleInner>,
}

// ─── Internal channel variants ─────────────────────────────────────────────────

pub(crate) enum ShutdownHandleInner {
    Mpsc(std::sync::mpsc::SyncSender<ShutdownDecision>),
    #[cfg(feature = "tokio")]
    Tokio(tokio::sync::oneshot::Sender<ShutdownDecision>),
}

#[derive(Debug)]
#[allow(dead_code)] // `reason` is consumed by the Windows backend
pub(crate) enum ShutdownDecision {
    Allow,
    Block { reason: Option<String> },
}

// ─── ShutdownHandle impl ───────────────────────────────────────────────────────

impl ShutdownHandle {
    /// Delay shutdown and show a reason string.
    ///
    /// On **Windows** the string appears in the shutdown-blocker UI (max ~512 chars,
    /// passed to `ShutdownBlockReasonCreate`).
    /// On **macOS** the string is ignored — the OS controls the UI.
    ///
    /// ---
    ///
    /// 延迟关机并显示原因字符串。
    ///
    /// **Windows**：字符串显示在关机阻止界面（最长约 512 字符，传递给
    /// `ShutdownBlockReasonCreate`）。**macOS**：字符串被忽略，界面由 OS 控制。
    pub fn block(mut self, reason: impl Into<String>) {
        self.send_inner(ShutdownDecision::Block { reason: Some(reason.into()) });
    }

    /// Allow the OS to proceed with shutdown.
    ///
    /// ---
    ///
    /// 允许操作系统继续执行关机。
    pub fn allow(mut self) {
        self.send_inner(ShutdownDecision::Allow);
    }

    fn send_inner(&mut self, decision: ShutdownDecision) {
        if let Some(inner) = self.inner.take() {
            match inner {
                ShutdownHandleInner::Mpsc(tx) => { let _ = tx.send(decision); }
                #[cfg(feature = "tokio")]
                ShutdownHandleInner::Tokio(tx) => { let _ = tx.send(decision); }
            }
        }
    }
}

impl Drop for ShutdownHandle {
    /// Dropping without an explicit decision defaults to `Allow`.
    ///
    /// ---
    ///
    /// 未显式决策直接丢弃时，默认执行 `Allow`。
    fn drop(&mut self) {
        self.send_inner(ShutdownDecision::Allow);
    }
}
