//! "Proximity — Players" window: lists players detected this session and lets you mark any of
//! them as ignored, so they don't trigger the proximity key press. Auto-refreshes while open.

use crate::win32_helpers::{create_control, dpi_for_window, register_and_create_dialog, scaled_font, wide};
use crate::{config, proximity};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static PLAYERS_HWND: AtomicIsize = AtomicIsize::new(0);
// Last rendered list lines — so the timer only rebuilds the listbox when contents change
// (rebuilding resets scroll/selection, which is jarring while the user scrolls a long list).
static LAST_RENDER: Mutex<Vec<String>> = Mutex::new(Vec::new());

// Fixed-width markers so the player name always starts at byte 9.
const MARK_ALERT: &str = "[ALERT ] ";
const MARK_IGNORE: &str = "[IGNORE] ";

pub(crate) unsafe fn show_players_dialog(parent: HWND) {
    let existing = PLAYERS_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }
    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
    let mut pr: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut pr);
    let hwnd = register_and_create_dialog(
        "CadencePlayers",
        "Proximity \u{2014} Players",
        players_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        pr.left + 24,
        pr.top + 24,
        320,
        420,
        parent,
        hinstance,
    );
    PLAYERS_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe fn refresh_list(hwnd: HWND) {
    // Always show the persisted ignore list (survives restarts), then this session's other
    // detections. Each group sorted so the order is stable between refreshes.
    let ignored = proximity::ignored_players();
    let detected = proximity::detected_players();
    let mut ig_sorted = ignored.clone();
    ig_sorted.sort();
    let mut det_only: Vec<String> =
        detected.into_iter().filter(|n| !ignored.iter().any(|x| x == n)).collect();
    det_only.sort();

    let mut lines: Vec<String> = Vec::with_capacity(ig_sorted.len() + det_only.len());
    for n in &ig_sorted {
        lines.push(format!("{}{}", MARK_IGNORE, n));
    }
    for n in &det_only {
        lines.push(format!("{}{}", MARK_ALERT, n));
    }

    // Only rebuild when the contents actually changed — otherwise leave the listbox (and the
    // user's scroll position) alone.
    {
        let mut last = LAST_RENDER.lock().unwrap();
        if *last == lines {
            return;
        }
        *last = lines.clone();
    }

    let list = GetDlgItem(hwnd, IDC_LIST_PLAYERS as i32);
    let top = SendMessageW(list, LB_GETTOPINDEX, 0, 0);
    let sel = SendMessageW(list, LB_GETCURSEL, 0, 0);
    SendMessageW(list, WM_SETREDRAW, 0, 0); // batch updates, avoid flicker
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    for line in &lines {
        SendMessageW(list, LB_ADDSTRING, 0, wide(line).as_ptr() as LPARAM);
    }
    if sel >= 0 {
        SendMessageW(list, LB_SETCURSEL, sel as WPARAM, 0);
    }
    if top > 0 {
        SendMessageW(list, LB_SETTOPINDEX, top as WPARAM, 0); // restore scroll position
    }
    SendMessageW(list, WM_SETREDRAW, 1, 0);
    InvalidateRect(list, std::ptr::null(), TRUE);
}

/// Name of the selected row, with the marker prefix stripped.
unsafe fn selected_name(hwnd: HWND) -> Option<String> {
    let list = GetDlgItem(hwnd, IDC_LIST_PLAYERS as i32);
    let sel = SendMessageW(list, LB_GETCURSEL, 0, 0);
    if sel < 0 {
        return None;
    }
    let len = SendMessageW(list, LB_GETTEXTLEN, sel as WPARAM, 0);
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    SendMessageW(list, LB_GETTEXT, sel as WPARAM, buf.as_mut_ptr() as LPARAM);
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    // Markers are 9 ASCII bytes; the name follows.
    Some(s.get(MARK_ALERT.len()..).unwrap_or("").to_string())
}

unsafe fn persist_ignore() {
    let mut c = config::cached_config();
    c.proximity_ignore = proximity::ignored_players();
    let _ = config::save_config(&c);
}

unsafe extern "system" fn players_wnd_proc(hwnd: HWND, msg: UINT, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = scaled_font(dpi_for_window(hwnd));

            create_control(
                hwnd, hinstance, font, "STATIC",
                "[IGNORE] persists across sessions; [ALERT] = seen this session. Select one and \u{201C}Toggle Ignore\u{201D}.",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 8, 300, 34, 0,
            );
            create_control(
                hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32,
                WS_EX_CLIENTEDGE as u32,
                10, 46, 300, 280, IDC_LIST_PLAYERS,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Toggle Ignore",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                10, 334, 120, 26, IDC_BTN_PLAYER_TOGGLE,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Clear List",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                138, 334, 84, 26, IDC_BTN_PLAYER_CLEAR,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Close",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                232, 334, 78, 26, IDC_BTN_PLAYER_CLOSE,
            );
            *LAST_RENDER.lock().unwrap() = Vec::new(); // force the first populate on (re)open
            refresh_list(hwnd);
            SetTimer(hwnd, TIMER_PLAYERS, 1500, None);
            0
        }
        WM_TIMER => {
            if w_param == TIMER_PLAYERS {
                refresh_list(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = LOWORD(w_param as u32);
            if id == IDC_BTN_PLAYER_TOGGLE {
                if let Some(name) = selected_name(hwnd) {
                    if !name.is_empty() {
                        proximity::toggle_ignore(&name);
                        persist_ignore();
                        refresh_list(hwnd);
                    }
                }
                return 0;
            } else if id == IDC_BTN_PLAYER_CLEAR {
                proximity::clear_detected();
                refresh_list(hwnd);
                return 0;
            } else if id == IDC_BTN_PLAYER_CLOSE {
                KillTimer(hwnd, TIMER_PLAYERS);
                DestroyWindow(hwnd);
                PLAYERS_HWND.store(0, Ordering::Release);
                return 0;
            }
            0
        }
        WM_CLOSE => {
            KillTimer(hwnd, TIMER_PLAYERS);
            DestroyWindow(hwnd);
            PLAYERS_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
