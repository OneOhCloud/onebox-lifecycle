//! Windows network monitoring backend — compiled only when the `network` feature is enabled.
//!
//! Uses `NotifyNetworkConnectivityHintChange` (Windows 8+) to track NCSI
//! connectivity state. The callback is invoked on a system thread and
//! forwards updates to the hidden sentinel window via `PostMessageW`.
//!
//! ---
//!
//! 使用 `NotifyNetworkConnectivityHintChange`（Windows 8+）追踪 NCSI 连接状态。
//! 回调在系统线程上触发，通过 `PostMessageW` 将更新转发至隐藏哨兵窗口。

use std::cell::RefCell;

use windows::Win32::{
    Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
    NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, NotifyNetworkConnectivityHintChange,
    },
    Networking::WinSock::{
        NL_NETWORK_CONNECTIVITY_HINT,
        NetworkConnectivityLevelHintConstrainedInternetAccess,
        NetworkConnectivityLevelHintInternetAccess,
    },
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

use crate::common::SystemEvent;

/// Custom window message: posted by the OS callback to the hidden window.
///
/// `WPARAM`: 1 = network up, 0 = network down.
///
/// ---
///
/// 自定义窗口消息：由 OS 回调发送至隐藏窗口。`WPARAM`：1 = 网络可用，0 = 不可用。
pub(super) const WM_SENTINEL_NETWORK_CHANGE: u32 = WM_APP + 2;

// ─── Setup / teardown ──────────────────────────────────────────────────────────

/// Register the connectivity-hint change callback. Returns the notification handle.
///
/// `initialnotification = true` delivers the current state immediately, ensuring
/// `last_network_up` is set before the first real change event fires.
///
/// Ref: [`NotifyNetworkConnectivityHintChange`](
/// https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-notifynetworkconnectivityhintchange)
///
/// ---
///
/// 注册连接性提示变更回调，返回通知句柄。
///
/// `initialnotification = true` 立即下发当前状态，确保首个真实变更事件触发前
/// `last_network_up` 已被设置。
pub(super) fn setup(hwnd: HWND) -> HANDLE {
    let mut handle: HANDLE = unsafe { std::mem::zeroed() };
    unsafe {
        let _ = NotifyNetworkConnectivityHintChange(
            Some(net_change_callback),
            Some(hwnd.0 as *const _), // caller_context = HWND
            true,
            &mut handle,
        );
    }
    handle
}

/// Cancel the notification and release the handle.
///
/// Ref: [`CancelMibChangeNotify2`](
/// https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-cancelmibchangenotify2)
///
/// ---
///
/// 取消通知并释放句柄。
pub(super) fn teardown(handle: HANDLE) {
    unsafe { let _ = CancelMibChangeNotify2(handle); }
}

// ─── OS callback ──────────────────────────────────────────────────────────────

/// Invoked by Windows on a system thread when network connectivity changes.
///
/// Maps `NL_NETWORK_CONNECTIVITY_HINT` to a boolean and posts
/// `WM_SENTINEL_NETWORK_CHANGE` to the hidden sentinel window. Deduplication
/// happens in `handle_change` on the message-loop thread.
///
/// Connectivity levels treated as "up":
/// - `NetworkConnectivityLevelHintInternetAccess` (3) — full internet
/// - `NetworkConnectivityLevelHintConstrainedInternetAccess` (4) — captive portal
///
/// Ref: [`NL_NETWORK_CONNECTIVITY_HINT`](
/// https://learn.microsoft.com/windows/win32/api/nldef/ns-nldef-nl_network_connectivity_hint)
///
/// ---
///
/// 网络连接性变化时 Windows 在系统线程上调用。
///
/// 将 `NL_NETWORK_CONNECTIVITY_HINT` 映射为布尔值并向隐藏窗口发送
/// `WM_SENTINEL_NETWORK_CHANGE`。去重逻辑在消息循环线程的 `handle_change` 中执行。
///
/// 视为"可用"的连接级别：
/// - `NetworkConnectivityLevelHintInternetAccess` (3)——完整互联网
/// - `NetworkConnectivityLevelHintConstrainedInternetAccess` (4)——强制门户
unsafe extern "system" fn net_change_callback(
    caller_context: *const core::ffi::c_void,
    hint: NL_NETWORK_CONNECTIVITY_HINT,
) {
    let up = hint.ConnectivityLevel == NetworkConnectivityLevelHintInternetAccess
        || hint.ConnectivityLevel == NetworkConnectivityLevelHintConstrainedInternetAccess;

    let hwnd = HWND(caller_context as *mut _);
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_SENTINEL_NETWORK_CHANGE,
            WPARAM(up as usize),
            LPARAM(0),
        );
    }
}

// ─── WM_SENTINEL_NETWORK_CHANGE handler ───────────────────────────────────────

/// Handle `WM_SENTINEL_NETWORK_CHANGE` on the message-loop thread.
///
/// Deduplicates events: only emits `NetworkUp` / `NetworkDown` when the
/// satisfied-status actually changes from the last known state.
///
/// ---
///
/// 在消息循环线程处理 `WM_SENTINEL_NETWORK_CHANGE`。
///
/// 去重：仅在 satisfied 状态相对上次已知状态发生变化时发出 `NetworkUp` / `NetworkDown`。
pub(super) fn handle_change(
    wparam: WPARAM,
    state_cell: &RefCell<super::WindowState>,
) -> LRESULT {
    let up = wparam.0 != 0;
    let mut st = state_cell.borrow_mut();
    if st.last_network_up != Some(up) {
        st.last_network_up = Some(up);
        if up {
            st.event_tx.send(SystemEvent::NetworkUp);
        } else {
            st.event_tx.send(SystemEvent::NetworkDown);
        }
    }
    LRESULT(0)
}
