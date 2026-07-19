use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, scale};
use crate::{sequence, storage};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static SAVE_DIALOG_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_save_dialog() {
    let existing = SAVE_DIALOG_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    // Center on screen
    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);
    let sw = 320;
    let sh = 120;
    let sx = (screen_w - sw) / 2;
    let sy = (screen_h - sh) / 2;

    let hwnd = register_and_create_dialog(
        "CadenceSaveDialog", "Save",
        save_dialog_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, sw, sh,
        std::ptr::null_mut(), hinstance,
    );
    SAVE_DIALOG_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn save_dialog_wnd_proc(
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

            // "Name:" label
            create_control(
                hwnd, hinstance, font, "STATIC", "Name:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 14, 42, 20, 0,
            );

            // Generate default name
            let default_name = generate_seq_name();
            let wname = wide(&default_name);

            // Name edit control
            let h_edit = CreateWindowExW(
                WS_EX_CLIENTEDGE as u32,
                wide("EDIT").as_ptr(),
                wname.as_ptr(),
                WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
                scale(56, dpi), scale(10, dpi), scale(244, dpi), scale(24, dpi),
                hwnd,
                IDC_EDIT_SEQ_NAME as u16 as usize as HMENU,
                hinstance,
                std::ptr::null_mut(),
            );
            SendMessageW(h_edit, WM_SETFONT, font as WPARAM, 1);
            // Select all text
            SendMessageW(h_edit, EM_SETSEL as u32, 0, -1isize as LPARAM);
            SetFocus(h_edit);

            // Save button
            create_control(
                hwnd, hinstance, font, "BUTTON", "Save",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                120, 50, 70, 28, IDC_BTN_SAVE_OK,
            );

            // Discard button
            create_control(
                hwnd, hinstance, font, "BUTTON", "Discard",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                200, 50, 70, 28, IDC_BTN_SAVE_CANCEL,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_SAVE_OK {
                // Get name from edit control
                let h_edit = GetDlgItem(hwnd, IDC_EDIT_SEQ_NAME as i32);
                let mut buf = [0u16; 128];
                let len = GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
                let name = String::from_utf16_lossy(&buf[..len as usize]).trim().to_string();

                if name.is_empty() {
                    let msg = wide("Please enter a name.");
                    let title = wide("Error");
                    MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
                    return 0;
                }

                // Save the pending events as a named sequence
                let events = lock_or_recover(&PENDING_EVENTS).take();
                if let Some(evts) = events {
                    let seq = sequence::Sequence::new(name, evts);
                    if let Err(e) = storage::save_sequence(&seq) {
                        eprintln!("[Cadence] Failed to save: {}", e);
                    }
                    refresh_bindings();
                    // Refresh sequences window if open
                    sequences::refresh_sequences_list();
                }

                DestroyWindow(hwnd);
                SAVE_DIALOG_HWND.store(0, Ordering::Release);
            } else if control_id == IDC_BTN_SAVE_CANCEL {
                // Discard pending events
                lock_or_recover(&PENDING_EVENTS).take();
                DestroyWindow(hwnd);
                SAVE_DIALOG_HWND.store(0, Ordering::Release);
            }
            0
        }
        WM_CLOSE => {
            // Treat close as cancel - keep events in LAST_EVENTS but don't save
            lock_or_recover(&PENDING_EVENTS).take();
            DestroyWindow(hwnd);
            SAVE_DIALOG_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

fn generate_seq_name() -> String {
    let count = storage::list_sequences().map(|v| v.len()).unwrap_or(0);
    format!("seq_{}", count + 1)
}
