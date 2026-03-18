//! Sleep handle and monitor-level types — compiled only when the `sleep` feature is enabled.
//! 睡眠句柄与监控级别类型——仅在启用 `sleep` feature 时编译。

// ─── SleepHandle ──────────────────────────────────────────────────────────────

/// Unblocks the OS after pre-sleep work is done.
///
/// Returned inside [`SystemEvent::WillHibernate`]. Call [`allow`](Self::allow)
/// when finished; dropping without calling it also unblocks the OS.
///
/// **Only emitted on macOS in [`SleepMonitorLevel::Deep`] mode.**
///
/// ---
///
/// 睡前工作完成后解除 OS 阻塞。
///
/// 随 [`SystemEvent::WillHibernate`] 一同返回。完成后调用 [`allow`](Self::allow)；
/// 不调用直接丢弃同样会解除阻塞。
///
/// **仅在 macOS [`SleepMonitorLevel::Deep`] 模式下发出。**
pub struct SleepHandle {
    pub(crate) inner: Option<std::sync::mpsc::SyncSender<()>>,
}

impl SleepHandle {
    /// Signal that pre-sleep work is complete; the OS may now proceed with sleep.
    ///
    /// ---
    ///
    /// 通知 OS 睡前工作已完成，可以继续睡眠。
    pub fn allow(mut self) {
        if let Some(tx) = self.inner.take() { let _ = tx.send(()); }
    }
}

impl Drop for SleepHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.inner.take() { let _ = tx.send(()); }
    }
}

// ─── SleepMonitorLevel ────────────────────────────────────────────────────────

/// Controls sleep-monitoring depth on macOS. Has no effect on Windows.
///
/// ---
///
/// 控制 macOS 上的睡眠监控深度，在 Windows 上无效。
#[derive(Clone, Debug)]
pub enum SleepMonitorLevel {
    /// **Default.** Subscribes to `NSWorkspaceWillSleepNotification` and
    /// `NSWorkspaceDidWakeNotification` via `NSWorkspace.notificationCenter`.
    /// No dedicated thread; no ability to delay sleep.
    ///
    /// ---
    ///
    /// **默认值。** 通过 `NSWorkspace.notificationCenter` 订阅
    /// `NSWorkspaceWillSleepNotification` 和 `NSWorkspaceDidWakeNotification`。
    /// 无独立线程，无法延迟睡眠。
    Standard,

    /// Uses `IORegisterForSystemPower` (IOKit) on a dedicated CFRunLoop thread.
    ///
    /// - Emits [`SystemEvent::WillHibernate`] with a [`SleepHandle`] before any sleep
    ///   transition, including hibernation. Does **not** emit [`SystemEvent::WillSleep`].
    /// - Still emits [`SystemEvent::DidWake`] after wake
    ///   (`kIOMessageSystemHasPoweredOn`).
    /// - The OS waits at most `timeout` for [`SleepHandle::allow`]; if the timeout
    ///   expires the OS proceeds regardless.
    ///
    /// ---
    ///
    /// 在独立 CFRunLoop 线程上调用 `IORegisterForSystemPower`（IOKit）。
    ///
    /// - 任何睡眠转换（含休眠）前发出带 [`SleepHandle`] 的 [`SystemEvent::WillHibernate`]，
    ///   **不**发出 [`SystemEvent::WillSleep`]。
    /// - 唤醒后（`kIOMessageSystemHasPoweredOn`）仍发出 [`SystemEvent::DidWake`]。
    /// - OS 最多等待 `timeout` 后强制继续睡眠。
    Deep {
        /// Maximum time to wait for [`SleepHandle::allow`] before the OS proceeds.
        ///
        /// ---
        ///
        /// OS 强制继续睡眠前等待 [`SleepHandle::allow`] 的最长时间。
        timeout: std::time::Duration,
    },
}

impl Default for SleepMonitorLevel {
    fn default() -> Self { SleepMonitorLevel::Standard }
}

impl SleepMonitorLevel {
    /// Returns [`Deep`](SleepMonitorLevel::Deep) with a 3-second timeout.
    ///
    /// ---
    ///
    /// 返回超时为 3 秒的 [`Deep`](SleepMonitorLevel::Deep) 模式。
    pub fn deep() -> Self {
        SleepMonitorLevel::Deep { timeout: std::time::Duration::from_secs(3) }
    }
}
