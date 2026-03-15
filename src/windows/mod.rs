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
///   OR caller calls handle.allow()           ← we return TRUE (OS proceeds)
///   → when cleanup done: call handle.allow()
///       → background thread calls ShutdownBlockReasonDestroy + optional ExitWindowsEx
/// ```
use std::sync::{Arc, Mutex};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        NetworkManagement::IpHelper::{
            CancelMibChangeNotify2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
            NotifyIpInterfaceChange,
        },
        Networking::WinSock::AF_UNSPEC,
        System::{
            Power::{PBT_APMRESUMESUSPEND, PBT_APMSUSPEND},
            Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy},
            Threading::GetCurrentThreadId,
        },
        UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, GetMessageW, HMENU, HWND_MESSAGE, MSG, PostMessageW,
            PostThreadMessageW, RegisterClassExW, TranslateMessage, UnregisterClassW,
            WINDOW_EX_STYLE, WM_APP, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_QUIT, WNDCLASSEXW,
            WS_OVERLAPPED,
        },
    },
    core::PCWSTR,
};

use crate::common::{
    EventSender, ShutdownDecision, ShutdownHandle, ShutdownHandleInner, SystemEvent,
};

// Custom message sent from the shutdown-decision callback back to the hidden window.
const WM_SENTINEL_ALLOW_SHUTDOWN: u32 = WM_APP + 1;
const WM_SENTINEL_NETWORK_CHANGE: u32 = WM_APP + 2;

// ─── Shared state passed into the window procedure ───────────────────────────

struct WindowState {
    event_tx: EventSender,
    /// Set when we are blocking a pending shutdown query.
    /// The HWND is stored so the background callback can send WM_SENTINEL_ALLOW_SHUTDOWN.
    pending_shutdown_hwnd: Option<HWND>,
}

// SAFETY: HWND is Send+Sync on Windows when used from the thread that created it.
// We pin all HWND usage to the message-loop thread, so this is safe here.
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
    // ── 1. Register a window class ──────────────────────────────────────────
    let class_name: Vec<u16> = "SysSentinelHidden\0".encode_utf16().collect();

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    RegisterClassExW(&wc);

    // ── 2. Create a message-only (hidden) window ────────────────────────────
    let state = Box::new(Mutex::new(WindowState {
        event_tx,
        pending_shutdown_hwnd: None,
    }));
    let state_ptr = Box::into_raw(state); // freed in WM_DESTROY

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR("SysSentinel\0".encode_utf16().collect::<Vec<_>>().as_ptr()),
        WS_OVERLAPPED,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        0,
        0,
        HWND_MESSAGE, // message-only window — never shown
        HMENU::default(),
        None,
        Some(state_ptr as *const _),
    )
    .expect("onebox_lifecycle: CreateWindowExW failed");

    // ── 3. Prioritise shutdown notification ─────────────────────────────────
    // 0x3FF = 1023: notified before most user apps.
    // Flag 0 = SHUTDOWN_NORETRY is NOT set here — we want to retry.
    let _ = windows::Win32::System::Shutdown::SetProcessShutdownParameters(0x3FF, 0);

    // ── 4. Register network-interface change callback ───────────────────────
    let mut net_notify_handle = std::mem::zeroed();
    let hwnd_clone = hwnd;
    let _ = NotifyIpInterfaceChange(
        AF_UNSPEC,
        Some(net_change_callback),
        Some(hwnd_clone.0 as *const _),
        false,
        &mut net_notify_handle,
    );

    // ── 5. Pump messages ────────────────────────────────────────────────────
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 == 0 || ret.0 == -1 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // ── 6. Cleanup ──────────────────────────────────────────────────────────
    let _ = CancelMibChangeNotify2(net_notify_handle);
    let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
}

// ─── Window procedure ─────────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Retrieve the state pointer stored in the window's user data.
    use windows::Win32::UI::WindowsAndMessaging::{
        CREATESTRUCTW, GWLP_USERDATA, GetWindowLongPtrW, SetWindowLongPtrW, WM_CREATE, WM_DESTROY,
        WM_NCCREATE,
    };

    if msg == WM_NCCREATE {
        // Stash the state pointer in GWLP_USERDATA at creation time.
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<WindowState>;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state_mutex = &*state_ptr;

    match msg {
        // ── Shutdown query ──────────────────────────────────────────────────
        WM_QUERYENDSESSION => {
            // Build a one-shot channel so the caller's ShutdownHandle can report
            // its decision back to us.
            let (decision_tx, decision_rx) = std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

            let handle = ShutdownHandle {
                inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
            };

            {
                let mut st = state_mutex.lock().unwrap();
                st.pending_shutdown_hwnd = Some(hwnd);
                st.event_tx.send(SystemEvent::ShuttingDown(handle));
            }

            // Wait (with a short timeout) for the caller to decide.
            // If they don't respond in time, we default to blocking — safer.
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
                    let _ = ShutdownBlockReasonCreate(hwnd, PCWSTR(wide.as_ptr()));

                    // Spawn a background task that watches for the eventual allow signal.
                    // We send ourselves WM_SENTINEL_ALLOW_SHUTDOWN when the user calls allow().
                    let hwnd_val = hwnd.0 as usize; // usize is Send
                    let state_ptr_usize = state_ptr as usize;
                    std::thread::spawn(move || {
                        // The caller still holds the *original* handle — they will call
                        // allow() when cleanup is done, which sends the decision on a
                        // *different* channel.  We can't observe that here directly.
                        //
                        // Pattern: the caller should call `ShutdownHandle::allow()`, which
                        // posts WM_SENTINEL_ALLOW_SHUTDOWN via the helper below.
                        // For demonstration, we poll every 200 ms for up to 5 minutes.
                        let hw = HWND(hwnd_val as *mut _);
                        for _ in 0..1500 {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            let st = unsafe { &*(state_ptr_usize as *mut Mutex<WindowState>) };
                            if st.lock().unwrap().pending_shutdown_hwnd.is_none() {
                                // Allow signal received via WM_SENTINEL_ALLOW_SHUTDOWN.
                                break;
                            }
                        }
                        // Whether timeout or allow, destroy the block reason so
                        // the OS can proceed on the next WM_QUERYENDSESSION.
                        unsafe {
                            let _ = ShutdownBlockReasonDestroy(hw);
                        }
                    });

                    LRESULT(0) // FALSE → block this round
                }
            }
        }

        // ── Async cleanup finished → clear pending shutdown ─────────────────
        WM_SENTINEL_ALLOW_SHUTDOWN => {
            let mut st = state_mutex.lock().unwrap();
            if let Some(hw) = st.pending_shutdown_hwnd.take() {
                let _ = ShutdownBlockReasonDestroy(hw);
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

        // ── Network change (custom message from callback) ───────────────────
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
            // Free the state box.
            drop(Box::from_raw(state_ptr));
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ─── Network change callback (called by the OS on a system thread) ────────────

unsafe extern "system" fn net_change_callback(
    caller_context: *const std::ffi::c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    // Determine connectivity (simplistic: any interface = up).
    // A more thorough impl would query GetIpInterfaceTable.
    let up = network_is_reachable();

    let hwnd = HWND(caller_context as *mut _);
    // Post back to the message-loop thread to avoid races.
    let _ = PostMessageW(
        hwnd,
        WM_SENTINEL_NETWORK_CHANGE,
        WPARAM(up as usize),
        LPARAM(0),
    );
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

/// Call this from your cleanup-complete handler to let the OS proceed with shutdown.
///
/// This is the Windows-side equivalent of `[NSApp replyToApplicationShouldTerminate: YES]`.
pub fn post_allow_shutdown(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(hwnd, WM_SENTINEL_ALLOW_SHUTDOWN, WPARAM(0), LPARAM(0));
    }
}
