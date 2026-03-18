//! macOS network monitoring backend — compiled only when the `network` feature is enabled.
//!
//! Uses `NWPathMonitor` (Network.framework, macOS 10.14+) via its C API.
//! The monitor delivers an initial path snapshot synchronously after
//! `nw_path_monitor_start`, so callers always get `NetworkUp` or `NetworkDown`
//! at startup without polling.
//!
//! ---
//!
//! 使用 Network.framework 的 `NWPathMonitor`（macOS 10.14+）C API。
//! `nw_path_monitor_start` 后同步下发初始路径快照，调用方无需轮询即可在
//! 启动时获得 `NetworkUp` 或 `NetworkDown`。

use std::sync::{Arc, Mutex};

use block2::RcBlock;

use crate::common::{EventSender, SystemEvent};

// ─── Network.framework C API ───────────────────────────────────────────────────
//
// `nw_*` objects are reference-counted via `nw_retain` / `nw_release` (not ObjC ARC).
// Ref: https://developer.apple.com/documentation/network/nwpathmonitor

mod nw_ffi {
    /// Opaque handle to a `nw_path_monitor_t`.
    pub type NwPathMonitorT = *mut std::ffi::c_void;
    /// Opaque handle to a `nw_path_t` (passed into the update-handler block).
    pub type NwPathT        = *mut std::ffi::c_void;

    /// `nw_path_status_satisfied` — at least one interface can carry traffic.
    /// Ref: [`nw_path_status_t`](https://developer.apple.com/documentation/network/nw_path_status_t)
    pub const NW_PATH_STATUS_SATISFIED: u32 = 1;

    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        /// Create a path monitor that observes all available interfaces.
        ///
        /// ---
        ///
        /// 创建监控所有可用接口的路径监视器。
        pub fn nw_path_monitor_create() -> NwPathMonitorT;

        /// Set the update-handler block; the monitor retains the block internally.
        ///
        /// ---
        ///
        /// 设置更新处理 block；监视器内部持有该 block。
        pub fn nw_path_monitor_set_update_handler(
            monitor:        NwPathMonitorT,
            update_handler: *const block2::Block<dyn Fn(NwPathT)>,
        );

        /// Set the dispatch queue on which the handler is invoked.
        ///
        /// ---
        ///
        /// 设置调用处理 block 的 dispatch 队列。
        pub fn nw_path_monitor_set_queue(
            monitor: NwPathMonitorT,
            queue:   *mut std::ffi::c_void,
        );

        /// Start observing; delivers the current path snapshot immediately.
        ///
        /// ---
        ///
        /// 开始监控；立即下发当前路径快照。
        pub fn nw_path_monitor_start(monitor: NwPathMonitorT);

        /// Stop observing and release OS resources.
        ///
        /// ---
        ///
        /// 停止监控并释放 OS 资源。
        pub fn nw_path_monitor_cancel(monitor: NwPathMonitorT);

        /// Query the satisfaction status of a `nw_path_t`.
        pub fn nw_path_get_status(path: NwPathT) -> u32; // nw_path_status_t

        /// Release a reference to any `nw_object_t` (including `nw_path_monitor_t`).
        pub fn nw_release(obj: *mut std::ffi::c_void);
    }

    // GCD — provided by libSystem, always linked on macOS.
    unsafe extern "C" {
        /// Returns the global concurrent queue at the given QoS class (0 = default).
        pub fn dispatch_get_global_queue(
            identifier: std::ffi::c_long,
            flags:      std::ffi::c_ulong,
        ) -> *mut std::ffi::c_void;
    }
}

// ─── PathMonitorGuard ──────────────────────────────────────────────────────────

/// RAII wrapper — cancels and releases the `NWPathMonitor` on drop.
///
/// ---
///
/// RAII 封装——析构时取消并释放 `NWPathMonitor`。
pub(super) struct PathMonitorGuard {
    monitor: nw_ffi::NwPathMonitorT,
    /// Kept alive so the block's captures (`Arc<EventSender>`) survive for the
    /// monitor's lifetime. The monitor also retains the block internally.
    ///
    /// ---
    ///
    /// 保持存活以确保 block 的捕获（`Arc<EventSender>`）在监视器生命周期内有效。
    /// 监视器内部也持有该 block。
    _block: RcBlock<dyn Fn(nw_ffi::NwPathT)>,
}

impl Drop for PathMonitorGuard {
    fn drop(&mut self) {
        // SAFETY: monitor was created by nw_path_monitor_create and has not
        // been cancelled before.
        unsafe {
            nw_ffi::nw_path_monitor_cancel(self.monitor);
            nw_ffi::nw_release(self.monitor);
        }
    }
}

// SAFETY: nw_path_monitor_t is thread-safe per Network.framework documentation.
// Ref: https://developer.apple.com/documentation/network/nwpathmonitor
unsafe impl Send for PathMonitorGuard {}
unsafe impl Sync for PathMonitorGuard {}

// ─── install ───────────────────────────────────────────────────────────────────

/// Start an `NWPathMonitor` and forward reachability changes to `event_tx`.
///
/// Events are deduplicated: only emitted when the satisfied-status changes,
/// or on the very first delivery (so callers always learn the initial state).
///
/// ---
///
/// 启动 `NWPathMonitor` 并将可达性变更转发至 `event_tx`。
///
/// 事件去重：仅在 satisfied 状态变化时或首次下发时发出，确保调用方始终获知初始状态。
pub(super) fn install(event_tx: &Arc<EventSender>) -> PathMonitorGuard {
    use nw_ffi::*;

    let event_tx = Arc::clone(event_tx);
    // `None` = never seen — first delivery always emits an event.
    let last_seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

    let block = RcBlock::new(move |path: NwPathT| {
        // SAFETY: `path` is a valid nw_path_t for the duration of this call.
        let satisfied = unsafe { nw_path_get_status(path) } == NW_PATH_STATUS_SATISFIED;

        let mut last = last_seen.lock().unwrap();
        if *last == Some(satisfied) { return; }
        *last = Some(satisfied);
        drop(last);

        event_tx.send(if satisfied { SystemEvent::NetworkUp } else { SystemEvent::NetworkDown });
    });

    let monitor = unsafe {
        let monitor = nw_path_monitor_create();
        assert!(!monitor.is_null(), "nw_path_monitor_create returned null");

        nw_path_monitor_set_update_handler(monitor, RcBlock::as_ptr(&block));

        // QOS_CLASS_DEFAULT = 0; flags must be 0 (reserved).
        let queue = dispatch_get_global_queue(0, 0);
        nw_path_monitor_set_queue(monitor, queue);

        nw_path_monitor_start(monitor);
        monitor
    };

    PathMonitorGuard { monitor, _block: block }
}
