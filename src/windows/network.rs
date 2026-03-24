//! Windows network monitoring backend — compiled only when the `network` feature is enabled.
//!
//! Uses `NotifyNetworkConnectivityHintChange` (Windows 10 2004+) to track NCSI
//! connectivity state. The callback is invoked on a system thread and
//! forwards updates to the hidden sentinel window via `PostMessageW`.
//!
//! On older systems (Windows 7 / 8 / 8.1) where the API is unavailable,
//! a warning is printed to stderr and network monitoring is skipped —
//! no crash, no network events.
//!
//! ---
//!
//! 使用 `NotifyNetworkConnectivityHintChange`（Windows 10 2004+）追踪 NCSI 连接状态。
//! 回调在系统线程上触发，通过 `PostMessageW` 将更新转发至隐藏哨兵窗口。
//!
//! 在不支持该 API 的旧系统（Windows 7 / 8 / 8.1）上，会向 stderr 输出警告
//! 并跳过网络监控——不会崩溃，也不会产生网络事件。

use std::cell::RefCell;

use windows::Win32::{
    Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
    NetworkManagement::IpHelper::{CancelMibChangeNotify2, NotifyNetworkConnectivityHintChange},
    Networking::WinSock::{
        NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityLevelHintConstrainedInternetAccess,
        NetworkConnectivityLevelHintInternetAccess,
    },
    System::LibraryLoader::{GetProcAddress, LoadLibraryW},
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

/// Register the connectivity-hint change callback if available (Windows 10 2004+).
///
/// Returns `Some(handle)` on success, or `None` if the API is not present
/// (e.g. Windows 7 / 8 / 8.1). Prints a diagnostic message to stderr in
/// both cases so the user can tell whether monitoring is active.
///
/// `initialnotification = true` delivers the current state immediately, ensuring
/// `last_network_up` is set before the first real change event fires.
///
/// Ref: [`NotifyNetworkConnectivityHintChange`](
/// https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-notifynetworkconnectivityhintchange)
///
/// ---
///
/// 注册连接性提示变更回调（需 Windows 10 2004+）。
///
/// 成功返回 `Some(handle)`；若 API 不存在（如 Windows 7 / 8 / 8.1）则返回 `None`。
/// 两种情况均向 stderr 输出诊断信息，以便用户判断监控是否生效。
///
/// `initialnotification = true` 立即下发当前状态，确保首个真实变更事件触发前
/// `last_network_up` 已被设置。
pub(super) fn setup(hwnd: HWND) -> Option<HANDLE> {
    if !api_available() {
        eprintln!(
            "onebox_lifecycle: NotifyNetworkConnectivityHintChange unavailable \
             (requires Windows 10 2004+) — network monitoring disabled"
        );
        return None;
    }

    let mut handle: HANDLE = unsafe { std::mem::zeroed() };
    unsafe {
        let _ = NotifyNetworkConnectivityHintChange(
            Some(net_change_callback),
            Some(hwnd.0 as *const _), // caller_context = HWND
            true,
            &mut handle,
        );
    }
    eprintln!(
        "onebox_lifecycle: network connectivity monitoring active \
         (NotifyNetworkConnectivityHintChange)"
    );
    Some(handle)
}

/// Check at runtime whether `iphlpapi.dll` exports
/// `NotifyNetworkConnectivityHintChange`.
///
/// Returns `true` on Windows 10 2004+, `false` on older systems.
///
/// ---
///
/// 运行时检查 `iphlpapi.dll` 是否导出 `NotifyNetworkConnectivityHintChange`。
///
/// Windows 10 2004+ 返回 `true`，旧系统返回 `false`。
fn api_available() -> bool {
    unsafe {
        let Ok(lib) = LoadLibraryW(windows::core::w!("iphlpapi.dll")) else {
            return false;
        };
        GetProcAddress(
            lib,
            windows::core::s!("NotifyNetworkConnectivityHintChange"),
        )
        .is_some()
    }
}

/// Cancel the notification and release the handle. No-op if `handle` is `None`.
///
/// Ref: [`CancelMibChangeNotify2`](
/// https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-cancelmibchangenotify2)
///
/// ---
///
/// 取消通知并释放句柄。`handle` 为 `None` 时不做任何操作。
pub(super) fn teardown(handle: Option<HANDLE>) {
    if let Some(h) = handle {
        unsafe {
            let _ = CancelMibChangeNotify2(h);
        }
    }
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
pub(super) fn handle_change(wparam: WPARAM, state_cell: &RefCell<super::WindowState>) -> LRESULT {
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common;

    /// Helper: create a `WindowState` wrapped in `RefCell` with a real channel.
    fn make_state() -> (RefCell<super::super::WindowState>, common::EventReceiver) {
        let (tx, rx) = common::channel();
        let state = RefCell::new(super::super::WindowState {
            event_tx: tx,
            last_network_up: None,
        });
        (state, rx)
    }

    // ── api_available ────────────────────────────────────────────────────────

    /// On Windows 10 2004+ (the CI and dev environment) the API must exist.
    /// This test documents the expectation; on an older VM it would need
    /// `#[ignore]`.
    #[test]
    fn api_probe_returns_true_on_supported_os() {
        assert!(
            api_available(),
            "Expected NotifyNetworkConnectivityHintChange to be present \
             on this OS — is this Windows 10 2004+?"
        );
    }

    // ── teardown(None) ───────────────────────────────────────────────────────

    #[test]
    fn teardown_none_is_noop() {
        // Must not panic or crash.
        teardown(None);
    }

    // ── handle_change deduplication ──────────────────────────────────────────

    #[test]
    fn first_up_event_emits_network_up() {
        let (state, rx) = make_state();
        let ret = handle_change(WPARAM(1), &state);
        assert_eq!(ret, LRESULT(0));
        assert!(matches!(rx.try_recv(), Some(SystemEvent::NetworkUp)));
    }

    #[test]
    fn first_down_event_emits_network_down() {
        let (state, rx) = make_state();
        let ret = handle_change(WPARAM(0), &state);
        assert_eq!(ret, LRESULT(0));
        assert!(matches!(rx.try_recv(), Some(SystemEvent::NetworkDown)));
    }

    #[test]
    fn duplicate_up_events_are_suppressed() {
        let (state, rx) = make_state();
        handle_change(WPARAM(1), &state);
        let _ = rx.try_recv(); // drain first event

        // Second identical event — should be suppressed.
        handle_change(WPARAM(1), &state);
        assert!(
            rx.try_recv().is_none(),
            "Duplicate NetworkUp should not be emitted"
        );
    }

    #[test]
    fn duplicate_down_events_are_suppressed() {
        let (state, rx) = make_state();
        handle_change(WPARAM(0), &state);
        let _ = rx.try_recv();

        handle_change(WPARAM(0), &state);
        assert!(
            rx.try_recv().is_none(),
            "Duplicate NetworkDown should not be emitted"
        );
    }

    #[test]
    fn toggle_up_down_emits_both() {
        let (state, rx) = make_state();

        handle_change(WPARAM(1), &state); // up
        handle_change(WPARAM(0), &state); // down
        handle_change(WPARAM(1), &state); // up again

        assert!(matches!(rx.try_recv(), Some(SystemEvent::NetworkUp)));
        assert!(matches!(rx.try_recv(), Some(SystemEvent::NetworkDown)));
        assert!(matches!(rx.try_recv(), Some(SystemEvent::NetworkUp)));
        assert!(rx.try_recv().is_none());
    }
}
