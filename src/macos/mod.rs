//! macOS platform backend.
//!
//! Assembles the feature submodules into [`MacosGuard`], which holds all RAII
//! resources for the lifetime of the sentinel.
//!
//! **Must be initialised from the main thread** (required by `NSApplicationDelegate`
//! and `NSNotificationCenter`; `NWPathMonitor` is safe from any thread but we
//! assert the main thread for consistency).
//!
//! ---
//!
//! macOS 平台后端。
//!
//! 将各 feature 子模块组装进 [`MacosGuard`]，由其持有哨兵生命周期内的所有 RAII 资源。
//!
//! **必须从主线程初始化**（`NSApplicationDelegate` 和 `NSNotificationCenter` 要求；
//! `NWPathMonitor` 虽然线程无关，但统一在主线程断言以保持一致性）。

use std::sync::Arc;

use objc2::MainThreadMarker;

use crate::common::EventSender;

// ─── Feature submodules ────────────────────────────────────────────────────────

#[cfg(feature = "shutdown")]
pub(super) mod shutdown;

#[cfg(feature = "sleep")]
pub(super) mod sleep;

#[cfg(feature = "network")]
pub(super) mod network;

// ─── MacosGuard ───────────────────────────────────────────────────────────────

/// Holds all macOS RAII resources. Dropping this struct cleans up all listeners.
///
/// ---
///
/// 持有所有 macOS RAII 资源。析构时清理所有监听器。
pub struct MacosGuard {
    #[cfg(feature = "shutdown")]
    _delegate: objc2::rc::Retained<shutdown::SentinelDelegate>,

    #[cfg(feature = "sleep")]
    _power_observer: objc2::rc::Retained<sleep::PowerObserver>,

    /// Declared after `_power_observer` so `removeObserver:` is called while
    /// `NSNotificationCenter` still holds a strong reference to the observer.
    ///
    /// ---
    ///
    /// 在 `_power_observer` 之后声明，确保 `NSNotificationCenter` 仍持有观察者强引用时
    /// 调用 `removeObserver:`。
    #[cfg(feature = "sleep")]
    _notification_guard: sleep::NotificationObserverGuard,

    /// Cancels `NWPathMonitor` on drop.
    #[cfg(feature = "network")]
    _path_monitor: network::PathMonitorGuard,
}

impl MacosGuard {
    /// Install all listeners. **Must be called from the main thread.**
    ///
    /// ---
    ///
    /// 安装所有监听器。**必须从主线程调用。**
    pub fn new(event_tx: EventSender) -> Self {
        #[allow(unused_variables)]
        let mtm = MainThreadMarker::new()
            .expect("onebox_lifecycle: MacosGuard::new() must be called from the main thread");

        // Wrap in Arc so it can be shared across submodule install calls
        // without moving or cloning the underlying channel sender.
        //
        // 用 Arc 包裹，便于在子模块 install 调用间共享，无需移动或克隆底层通道发送端。
        let event_tx = Arc::new(event_tx);

        #[cfg(feature = "shutdown")]
        let _delegate = shutdown::install(mtm, &event_tx);

        #[cfg(feature = "sleep")]
        let (_power_observer, _notification_guard) =
            sleep::install(mtm, &event_tx);

        #[cfg(feature = "network")]
        let _path_monitor = network::install(&event_tx);

        MacosGuard {
            #[cfg(feature = "shutdown")]
            _delegate,
            #[cfg(feature = "sleep")]
            _power_observer,
            #[cfg(feature = "sleep")]
            _notification_guard,
            #[cfg(feature = "network")]
            _path_monitor,
        }
    }
}
