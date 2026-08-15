use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, scale};
use crate::storage;
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static SET_GROUP_DIALOG_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_set_group_dialog(parent: HWND) {
    let existing = SET_GROUP_DIALOG_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);
    let sx = parent_rect.left + 50;
    let sy = parent_rect.top + 150;

    let hwnd = register_and_create_dialog(
        "CadenceGroupDialog", "Set Group",
        set_group_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 320, 140,
        parent, hinstance,
    );
    SET_GROUP_DIALOG_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn set_group_wnd_proc(
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

            create_control(
                hwnd, hinstance, font, "STATIC", "Group:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 14, 42, 20, 0,
            );

            // Pre-fill with the first selected sequence's group; the entered group is
            // applied to every selected sequence on OK.
            let current_group = {
                let guard = lock_or_recover(&SET_GROUP_SEQ_NAMES);
                guard
                    .first()
                    .and_then(|filename| storage::load_sequence(filename).ok())
                    .and_then(|seq| seq.group)
                    .unwrap_or_default()
            };

            let wgroup = wide(&current_group);
            let h_edit = CreateWindowExW(
                WS_EX_CLIENTEDGE as u32,
                wide("EDIT").as_ptr(),
                wgroup.as_ptr(),
                WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
                scale(56, dpi), scale(10, dpi), scale(244, dpi), scale(24, dpi),
                hwnd,
                IDC_EDIT_SEQ_NAME as u16 as usize as HMENU,
                hinstance,
                std::ptr::null_mut(),
            );
            SendMessageW(h_edit, WM_SETFONT, font as WPARAM, 1);
            SendMessageW(h_edit, EM_SETSEL as u32, 0, -1isize as LPARAM);
            SetFocus(h_edit);

            create_control(
                hwnd, hinstance, font, "STATIC", "Leave empty to remove from group.",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 40, 290, 16, 0,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                120, 65, 70, 28, IDC_BTN_SAVE_OK,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                200, 65, 70, 28, IDC_BTN_SAVE_CANCEL,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_SAVE_OK {
                let h_edit = GetDlgItem(hwnd, IDC_EDIT_SEQ_NAME as i32);
                let mut buf = [0u16; 128];
                let len = GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
                let new_group = String::from_utf16_lossy(&buf[..len as usize]).trim().to_string();

                let seq_filenames = lock_or_recover(&SET_GROUP_SEQ_NAMES).clone();
                for filename in &seq_filenames {
                    match storage::load_sequence(filename) {
                        Ok(mut seq) => {
                            seq.group = if new_group.is_empty() {
                                None
                            } else {
                                Some(new_group.clone())
                            };
                            if let Err(e) = storage::save_sequence(&seq) {
                                eprintln!("[Cadence] Failed to save group: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("[Cadence] Failed to load sequence for group update: {}", e);
                        }
                    }
                }
                if !seq_filenames.is_empty() {
                    sequences::refresh_sequences_list();
                }

                DestroyWindow(hwnd);
                SET_GROUP_DIALOG_HWND.store(0, Ordering::Release);
            } else if control_id == IDC_BTN_SAVE_CANCEL {
                DestroyWindow(hwnd);
                SET_GROUP_DIALOG_HWND.store(0, Ordering::Release);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            SET_GROUP_DIALOG_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
