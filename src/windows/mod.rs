/// Windows platform backend.
///
/// Spawns a dedicated OS thread that owns a hidden `HWND` and runs
/// `GetMessageW` forever.  All system events arrive as window messages.
///
/// # Shutdown flow
///
/// ```text
/// WM_QUERYENDSESSION
///   → send SystemEvent::ShuttingDown(handle)
///   → caller calls handle.block("reason")   ← we return FALSE + ShutdownBlockReasonCreate
///                                              background thread waits on condvar
///   OR caller calls handle.allow()           ← we return TRUE (OS proceeds)
///   → when cleanup done: call post_allow_shutdown(hwnd)
///       → WM_SENTINEL_ALLOW_SHUTDOWN handler signals condvar
///       → background thread wakes and calls ShutdownBlockReasonDestroy
///       → OS re-sends WM_QUERYENDSESSION; this time allow() → TRUE
/// ```
use std::sync::{Arc, Condvar, Mutex};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        NetworkManagement::IpHelper::{
            CancelMibChangeNotify2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
            NotifyIpInterfaceChange,
        },
        Networking::WinSock::AF_UNSPEC,
        System::{
            Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy},
            Threading::SetProcessShutdownParameters,
        },
        UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
            DispatchMessageW, GetMessageW, HMENU, HWND_MESSAGE, MSG, PostMessageW,
            RegisterClassExW, TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WM_APP,
            WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
        },
    },
    core::{PCWSTR, w},
};

use crate::common::{
    EventSender, ShutdownDecision, ShutdownHandle, ShutdownHandleInner, SystemEvent,
};

// Custom messages sent back to the hidden window from other threads/callbacks.
const WM_SENTINEL_ALLOW_SHUTDOWN: u32 = WM_APP + 1;
const WM_SENTINEL_NETWORK_CHANGE: u32 = WM_APP + 2;

// These power-event codes are not re-exported as named constants in windows-rs.
const PBT_APMSUSPEND: u32 = 4;
const PBT_APMRESUMESUSPEND: u32 = 7;

// Compile-time UTF-16 class name — no heap allocation.
const CLASS_NAME: PCWSTR = w!("SysSentinelHidden");

// ─── Shared state passed into the window procedure ───────────────────────────

struct WindowState {
    event_tx: EventSender,
    /// Set while we are blocking a pending shutdown query.
    pending_shutdown_hwnd: Option<HWND>,
    /// Signals the background watcher thread that `allow()` has been called,
    /// so it can invoke `ShutdownBlockReasonDestroy` and exit.
    shutdown_notify: Option<Arc<(Mutex<bool>, Condvar)>>,
}

// SAFETY: HWND is Send+Sync on Windows when used from the thread that created it.
// All HWND usage is pinned to the message-loop thread; the raw pointer in
// `shutdown_notify` is never dereferenced across threads.
unsafe impl Send for WindowState {}
unsafe impl Sync for WindowState {}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Start the Windows sentinel on a background thread.
///
/// Returns immediately.  Events arrive on the [`EventSender`] channel.
pub fn start(event_tx: EventSender) {
    std::thread::Builder::new()
        .name("onebox_lifecycle_win32".into())
        .spawn(move || {
            // SAFETY: whole message loop is on this single thread.
            unsafe { run_message_loop(event_tx) };
        })
        .expect("onebox_lifecycle: failed to spawn Win32 message thread");
}

unsafe fn run_message_loop(event_tx: EventSender) {
    unsafe {
        // ── 1. Register a window class ──────────────────────────────────────
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // ── 2. Create a message-only (hidden) window ────────────────────────
        let state = Box::new(Mutex::new(WindowState {
            event_tx,
            pending_shutdown_hwnd: None,
            shutdown_notify: None,
        }));
        let state_ptr = Box::into_raw(state); // freed in WM_DESTROY

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("SysSentinel"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            Some(HWND_MESSAGE), // message-only window — never shown
            Some(HMENU::default()),
            None,
            Some(state_ptr as *const _),
        )
        .expect("onebox_lifecycle: CreateWindowExW failed");

        // ── 3. Prioritise shutdown notification ─────────────────────────────
        // Level 0x3FF = 1023: our process is notified before most user apps.
        // Flags = 0: SHUTDOWN_NORETRY is NOT set (we want OS to retry after we unblock).
        let _ = SetProcessShutdownParameters(0x3FF, 0);

        // ── 4. Register network-interface change callback ───────────────────
        let mut net_notify_handle = std::mem::zeroed();
        let _ = NotifyIpInterfaceChange(
            AF_UNSPEC,
            Some(net_change_callback),
            // Pass HWND.0 (*mut c_void) as the opaque context pointer.
            Some(hwnd.0 as *const _),
            false,
            &mut net_notify_handle,
        );

        // ── 5. Pump messages ────────────────────────────────────────────────
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // ── 6. Cleanup ──────────────────────────────────────────────────────
        let _ = CancelMibChangeNotify2(net_notify_handle);
        let _ = UnregisterClassW(CLASS_NAME, None);
    }
}

// ─── Window procedure ─────────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CREATESTRUCTW, GWLP_USERDATA, GetWindowLongPtrW, SetWindowLongPtrW, WM_DESTROY, WM_NCCREATE,
    };

    if msg == WM_NCCREATE {
        unsafe {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Mutex<WindowState>;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state_mutex = unsafe { &*state_ptr };

    match msg {
        // ── Shutdown query ──────────────────────────────────────────────────
        WM_QUERYENDSESSION => {
            let (decision_tx, decision_rx) = std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

            let handle = ShutdownHandle {
                inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
            };

            {
                let mut st = state_mutex.lock().unwrap();
                st.pending_shutdown_hwnd = Some(hwnd);
                st.event_tx.send(SystemEvent::ShuttingDown(handle));
            }

            // Wait up to 2 s for the caller to decide.  Default to blocking
            // (safer than silently allowing shutdown mid-cleanup).
            let decision = decision_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap_or(ShutdownDecision::Block {
                    reason: Some("onebox_lifecycle: cleanup in progress".into()),
                });

            match decision {
                ShutdownDecision::Allow => {
                    state_mutex.lock().unwrap().pending_shutdown_hwnd = None;
                    LRESULT(1) // TRUE → allow
                }
                ShutdownDecision::Block { reason } => {
                    let reason_str = reason.unwrap_or_else(|| "Cleanup in progress…".into());
                    let wide: Vec<u16> = reason_str
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect();
                    let _ = unsafe { ShutdownBlockReasonCreate(hwnd, PCWSTR(wide.as_ptr())) };

                    // Set up a condvar so WM_SENTINEL_ALLOW_SHUTDOWN can wake
                    // the background thread without polling.
                    let notify = Arc::new((Mutex::new(false), Condvar::new()));
                    let notify_clone = Arc::clone(&notify);
                    state_mutex.lock().unwrap().shutdown_notify = Some(notify);

                    // hwnd.0 is *mut c_void; cast to usize for Send across threads.
                    let hwnd_usize = hwnd.0 as usize;
                    std::thread::spawn(move || {
                        let (lock, cvar) = &*notify_clone;
                        let guard = lock.lock().unwrap();
                        // Block until allow() is signalled or 5-minute safety timeout.
                        let _ = cvar.wait_timeout_while(
                            guard,
                            std::time::Duration::from_secs(300),
                            |&mut done| !done,
                        );
                        // Only this thread calls ShutdownBlockReasonDestroy.
                        unsafe {
                            let _ = ShutdownBlockReasonDestroy(HWND(hwnd_usize as *mut _));
                        }
                    });

                    LRESULT(0) // FALSE → block this round
                }
            }
        }

        // ── Async cleanup finished → signal the watcher thread ─────────────
        WM_SENTINEL_ALLOW_SHUTDOWN => {
            let mut st = state_mutex.lock().unwrap();
            st.pending_shutdown_hwnd = None;
            if let Some(notify) = st.shutdown_notify.take() {
                let (lock, cvar) = &*notify;
                *lock.lock().unwrap() = true;
                cvar.notify_one();
            }
            LRESULT(0)
        }

        // ── Power events ────────────────────────────────────────────────────
        WM_POWERBROADCAST => {
            let event_code = wparam.0 as u32;
            let st = state_mutex.lock().unwrap();
            match event_code {
                PBT_APMSUSPEND => st.event_tx.send(SystemEvent::WillSleep),
                PBT_APMRESUMESUSPEND => st.event_tx.send(SystemEvent::DidWake),
                _ => {}
            }
            LRESULT(1)
        }

        // ── Network change (custom message posted by the OS callback) ───────
        WM_SENTINEL_NETWORK_CHANGE => {
            let up = wparam.0 != 0;
            let st = state_mutex.lock().unwrap();
            if up {
                st.event_tx.send(SystemEvent::NetworkUp);
            } else {
                st.event_tx.send(SystemEvent::NetworkDown);
            }
            LRESULT(0)
        }

        // ── Teardown ────────────────────────────────────────────────────────
        WM_DESTROY => {
            drop(unsafe { Box::from_raw(state_ptr) });
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Network change callback (called by the OS on a system thread) ────────────

unsafe extern "system" fn net_change_callback(
    caller_context: *const core::ffi::c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    let up = network_is_reachable();
    // Reconstruct the HWND from the opaque context pointer we passed at registration.
    let hwnd = HWND(caller_context as *mut _);
    // Post to the message-loop thread to avoid data races on WindowState.
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_SENTINEL_NETWORK_CHANGE,
            WPARAM(up as usize),
            LPARAM(0),
        )
    };
}

fn network_is_reachable() -> bool {
    use windows::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIpInterfaceTable, MIB_IPINTERFACE_TABLE,
    };
    use windows::Win32::Networking::WinSock::AF_INET;

    unsafe {
        let mut table: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();
        if GetIpInterfaceTable(AF_INET, &mut table).is_ok() {
            let count = (*table).NumEntries;
            FreeMibTable(table as *mut _);
            return count > 0;
        }
        false
    }
}

// ─── Public helper: post the "allow shutdown" message from any thread ─────────

/// Call this once your async cleanup is complete to let the OS proceed with shutdown.
///
/// This posts [`WM_SENTINEL_ALLOW_SHUTDOWN`] to the hidden sentinel window, which
/// wakes the background watcher thread and causes it to call
/// `ShutdownBlockReasonDestroy`.  The OS will then re-issue `WM_QUERYENDSESSION`
/// and the next [`ShutdownHandle::allow`] call will return `TRUE`.
///
/// Equivalent to `[NSApp replyToApplicationShouldTerminate: YES]` on macOS.

#[allow(dead_code)]
pub fn post_allow_shutdown(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SENTINEL_ALLOW_SHUTDOWN, WPARAM(0), LPARAM(0));
    }
}
