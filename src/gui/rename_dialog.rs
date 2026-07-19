use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, scale};
use crate::{config, storage};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static RENAME_DIALOG_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_rename_dialog(parent: HWND) {
    let existing = RENAME_DIALOG_HWND.load(Ordering::Acquire) as HWND;
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
        "CadenceRenameDialog", "Rename",
        rename_dialog_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 320, 120,
        parent, hinstance,
    );
    RENAME_DIALOG_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn rename_dialog_wnd_proc(
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
                hwnd, hinstance, font, "STATIC", "Name:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 14, 42, 20, 0,
            );

            let current_display_name = {
                let name_guard = lock_or_recover(&RENAME_SEQ_NAME);
                if let Some(ref filename) = *name_guard {
                    storage::load_sequence(filename)
                        .map(|seq| seq.name)
                        .unwrap_or_else(|_| filename.clone())
                } else {
                    String::new()
                }
            };

            let wname = wide(&current_display_name);
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
            SendMessageW(h_edit, EM_SETSEL as u32, 0, -1isize as LPARAM);
            SetFocus(h_edit);

            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                120, 50, 70, 28, IDC_BTN_SAVE_OK,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                200, 50, 70, 28, IDC_BTN_SAVE_CANCEL,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_SAVE_OK {
                let h_edit = GetDlgItem(hwnd, IDC_EDIT_SEQ_NAME as i32);
                let mut buf = [0u16; 128];
                let len = GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
                let new_name = String::from_utf16_lossy(&buf[..len as usize]).trim().to_string();

                if new_name.is_empty() {
                    let msg = wide("Please enter a name.");
                    let title = wide("Error");
                    MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
                    return 0;
                }

                let old_filename = lock_or_recover(&RENAME_SEQ_NAME).clone();

                if let Some(old_filename) = old_filename {
                    match storage::rename_sequence(&old_filename, &new_name) {
                        Ok(()) => {
                            let new_sanitized = storage::sanitize_filename(&new_name);

                            let mut cfg = config::load_config();
                            if cfg.default_sequence.as_deref() == Some(&old_filename) {
                                cfg.default_sequence = Some(new_sanitized.clone());
                                let _ = config::save_config(&cfg);
                            }

                            {
                                let mut queue = lock_or_recover(&SEQUENCE_QUEUE);
                                for entry in queue.iter_mut() {
                                    if *entry == old_filename {
                                        *entry = new_sanitized.clone();
                                    }
                                }
                            }

                            refresh_bindings();
                            sequences::refresh_sequences_list();

                            DestroyWindow(hwnd);
                            RENAME_DIALOG_HWND.store(0, Ordering::Release);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            let msg = wide(&format!(
                                "A sequence named \"{}\" already exists. Choose a different name.",
                                new_name
                            ));
                            let title = wide("Name Conflict");
                            MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING);
                        }
                        Err(e) => {
                            let msg = wide(&format!("Failed to rename: {}", e));
                            let title = wide("Error");
                            MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
                            DestroyWindow(hwnd);
                            RENAME_DIALOG_HWND.store(0, Ordering::Release);
                        }
                    }
                } else {
                    DestroyWindow(hwnd);
                    RENAME_DIALOG_HWND.store(0, Ordering::Release);
                }

                0
            } else if control_id == IDC_BTN_SAVE_CANCEL {
                DestroyWindow(hwnd);
                RENAME_DIALOG_HWND.store(0, Ordering::Release);
                0
            } else {
                0
            }
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            RENAME_DIALOG_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
