//! Saved Queues: name the current queue and keep it on disk (`%APPDATA%\ranify2\queues\`), or
//! load / append one back. Unlike a group, a saved queue keeps order AND repeats, and one
//! sequence may sit in any number of saved queues — so nothing has to be duplicated to be played
//! twice.

use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, scale};
use crate::{sequence, storage};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static SAVED_QUEUES_HWND: AtomicIsize = AtomicIsize::new(0);
// Row index -> saved queue name (rows show extra text, so the name is kept aside).
static ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub unsafe fn show_saved_queues_dialog(parent: HWND) {
    let existing = SAVED_QUEUES_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);
    let sx = parent_rect.left + 120;
    let sy = parent_rect.top + 60;

    let hwnd = register_and_create_dialog(
        "CadenceSavedQueues", "Saved Queues",
        saved_queues_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 350, 350,
        parent, hinstance,
    );
    SAVED_QUEUES_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe fn close(hwnd: HWND) {
    DestroyWindow(hwnd);
    SAVED_QUEUES_HWND.store(0, Ordering::Release);
}

unsafe fn read_name(hwnd: HWND) -> String {
    let h_edit = GetDlgItem(hwnd, IDC_EDIT_SAVED_QUEUE_NAME as i32);
    let mut buf = [0u16; 128];
    let len = GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..len as usize]).trim().to_string()
}

/// The saved queue the user means: the typed name, else the selected row.
unsafe fn target_name(hwnd: HWND) -> Option<String> {
    let typed = read_name(hwnd);
    if !typed.is_empty() {
        return Some(typed);
    }
    let h_list = GetDlgItem(hwnd, IDC_LIST_SAVED_QUEUES as i32);
    let sel = SendMessageW(h_list, LB_GETCURSEL, 0, 0);
    if sel < 0 {
        return None;
    }
    lock_or_recover(&ROWS).get(sel as usize).cloned()
}

unsafe fn warn(hwnd: HWND, text: &str, title: &str) {
    MessageBoxW(hwnd, wide(text).as_ptr(), wide(title).as_ptr(), MB_OK | MB_ICONWARNING);
}

/// Re-list every saved queue as "name (N items, m:ss)"; keeps `select` selected if given.
unsafe fn refresh_list(hwnd: HWND, select: Option<&str>) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_SAVED_QUEUES as i32);
    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    let names = storage::list_saved_queues();
    let mut sel_idx: Option<usize> = None;
    for (i, name) in names.iter().enumerate() {
        let display = match storage::load_saved_queue(name) {
            Ok(q) => {
                let timings: Vec<(i64, i64)> = q
                    .items
                    .iter()
                    .map(|n| {
                        storage::load_sequence(n)
                            .map(|s| (s.duration_micros(), s.leading_delay_micros()))
                            .unwrap_or((0, 0))
                    })
                    .collect();
                format!(
                    "{} ({} item{}, {})",
                    name,
                    q.items.len(),
                    if q.items.len() == 1 { "" } else { "s" },
                    sequence::format_duration(sequence::queue_pass_micros(&timings)),
                )
            }
            Err(_) => format!("{} (unreadable)", name),
        };
        SendMessageW(h_list, LB_ADDSTRING, 0, wide(&display).as_ptr() as LPARAM);
        if select == Some(name.as_str()) {
            sel_idx = Some(i);
        }
    }
    *lock_or_recover(&ROWS) = names;
    if let Some(i) = sel_idx {
        SendMessageW(h_list, LB_SETCURSEL, i as WPARAM, 0);
    }
}

unsafe fn handle_save(hwnd: HWND) {
    let name = read_name(hwnd);
    if name.is_empty() {
        warn(hwnd, "Enter a name for the saved queue.", "Saved Queues");
        return;
    }
    let items = lock_or_recover(&SEQUENCE_QUEUE).clone();
    if items.is_empty() {
        warn(hwnd, "The queue is empty \u{2014} add sequences to it first.", "Saved Queues");
        return;
    }
    if storage::saved_queue_exists(&name) {
        let msg = wide(&format!("Replace the saved queue \"{}\"?", name));
        if MessageBoxW(hwnd, msg.as_ptr(), wide("Saved Queues").as_ptr(), MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }
    match storage::save_saved_queue(&name, items) {
        Ok(q) => {
            *lock_or_recover(&QUEUE_LABEL) = Some(q.name.clone());
            sequences::notify_queue_changed(); // caption now carries the name
            refresh_list(hwnd, Some(&q.name));
        }
        Err(e) => warn(hwnd, &format!("Couldn't save the queue: {}", e), "Saved Queues"),
    }
}

unsafe fn handle_load(hwnd: HWND, append: bool) {
    let Some(name) = target_name(hwnd) else {
        warn(hwnd, "Select a saved queue first.", "Saved Queues");
        return;
    };
    let q = match storage::load_saved_queue(&name) {
        Ok(q) => q,
        Err(_) => {
            warn(hwnd, &format!("No saved queue named \"{}\".", name), "Saved Queues");
            return;
        }
    };
    if append {
        edit_queue(|queue| queue.extend(q.items));
        sequences::notify_queue_changed();
    } else {
        set_queue(q.items, Some(q.name));
    }
}

unsafe fn handle_delete(hwnd: HWND) {
    let Some(name) = target_name(hwnd) else {
        warn(hwnd, "Select a saved queue first.", "Saved Queues");
        return;
    };
    let msg = wide(&format!("Delete the saved queue \"{}\"?\n(Sequences are not touched.)", name));
    if MessageBoxW(hwnd, msg.as_ptr(), wide("Saved Queues").as_ptr(), MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    if let Err(e) = storage::delete_saved_queue(&name) {
        warn(hwnd, &format!("Couldn't delete: {}", e), "Saved Queues");
        return;
    }
    let mut label = lock_or_recover(&QUEUE_LABEL);
    if label.as_deref() == Some(name.as_str()) {
        *label = None;
        drop(label);
        sequences::notify_queue_changed();
    }
    SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_SAVED_QUEUE_NAME as i32), wide("").as_ptr());
    refresh_list(hwnd, None);
}

unsafe extern "system" fn saved_queues_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let dpi = dpi_for_window(hwnd);
            let font = scaled_font(dpi);

            create_control(hwnd, hinstance, font, "STATIC", "Saved queues:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 10, 8, 200, 16, 0);
            create_control(hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32,
                WS_EX_CLIENTEDGE as u32, 10, 26, 320, 150, IDC_LIST_SAVED_QUEUES);

            create_control(hwnd, hinstance, font, "STATIC", "Name:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 10, 187, 42, 20, 0);
            let h_name = CreateWindowExW(
                WS_EX_CLIENTEDGE as u32,
                wide("EDIT").as_ptr(),
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
                scale(56, dpi), scale(184, dpi), scale(274, dpi), scale(24, dpi),
                hwnd, IDC_EDIT_SAVED_QUEUE_NAME as usize as HMENU, hinstance, std::ptr::null_mut(),
            );
            SendMessageW(h_name, WM_SETFONT, font as WPARAM, 1);
            SendMessageW(h_name, EM_LIMITTEXT as u32, 100, 0);

            create_control(hwnd, hinstance, font, "BUTTON", "Save current queue as",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 10, 218, 160, 28, IDC_BTN_SQ_SAVE);
            create_control(hwnd, hinstance, font, "BUTTON", "Delete",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 250, 218, 80, 28, IDC_BTN_SQ_DELETE);

            create_control(hwnd, hinstance, font, "BUTTON", "Load (replace)",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 10, 254, 110, 28, IDC_BTN_SQ_LOAD);
            create_control(hwnd, hinstance, font, "BUTTON", "Append",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 126, 254, 80, 28, IDC_BTN_SQ_APPEND);
            create_control(hwnd, hinstance, font, "BUTTON", "Close",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 250, 254, 80, 28, IDC_BTN_SQ_CLOSE);

            create_control(hwnd, hinstance, font, "STATIC",
                "Keeps order and repeats; a sequence can be in many saved queues.",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 10, 290, 320, 20, 0);

            // Offer the loaded queue's name for a quick re-save.
            let label = queue_label();
            if let Some(ref l) = label {
                SetWindowTextW(h_name, wide(l).as_ptr());
            }
            refresh_list(hwnd, label.as_deref());
            SetFocus(h_name);
            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            let notify = HIWORD(w_param as u32);
            match control_id {
                x if x == IDC_BTN_SQ_SAVE => handle_save(hwnd),
                x if x == IDC_BTN_SQ_LOAD => handle_load(hwnd, false),
                x if x == IDC_BTN_SQ_APPEND => handle_load(hwnd, true),
                x if x == IDC_BTN_SQ_DELETE => handle_delete(hwnd),
                x if x == IDC_BTN_SQ_CLOSE => close(hwnd),
                x if x == IDC_LIST_SAVED_QUEUES => {
                    if notify == LBN_SELCHANGE || notify == LBN_DBLCLK {
                        let h_list = GetDlgItem(hwnd, IDC_LIST_SAVED_QUEUES as i32);
                        let sel = SendMessageW(h_list, LB_GETCURSEL, 0, 0);
                        if sel >= 0 {
                            if let Some(name) = lock_or_recover(&ROWS).get(sel as usize).cloned() {
                                SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_SAVED_QUEUE_NAME as i32),
                                    wide(&name).as_ptr());
                            }
                        }
                        if notify == LBN_DBLCLK {
                            handle_load(hwnd, false);
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            close(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
