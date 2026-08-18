use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, scale};
use crate::storage;
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static DUPLICATE_DIALOG_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_duplicate_dialog(parent: HWND) {
    let existing = DUPLICATE_DIALOG_HWND.load(Ordering::Acquire) as HWND;
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
        "CadenceDuplicateDialog", "Duplicate",
        duplicate_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 320, 175,
        parent, hinstance,
    );
    DUPLICATE_DIALOG_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe fn close(hwnd: HWND) {
    DestroyWindow(hwnd);
    DUPLICATE_DIALOG_HWND.store(0, Ordering::Release);
}

unsafe fn create_edit(hwnd: HWND, hinstance: HINSTANCE, font: HFONT, dpi: u32, y: i32, text: &str, id: u16) -> HWND {
    let wtext = wide(text);
    let h_edit = CreateWindowExW(
        WS_EX_CLIENTEDGE as u32,
        wide("EDIT").as_ptr(),
        wtext.as_ptr(),
        WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
        scale(56, dpi), scale(y, dpi), scale(244, dpi), scale(24, dpi),
        hwnd,
        id as usize as HMENU,
        hinstance,
        std::ptr::null_mut(),
    );
    SendMessageW(h_edit, WM_SETFONT, font as WPARAM, 1);
    h_edit
}

unsafe extern "system" fn duplicate_wnd_proc(
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

            let sources = lock_or_recover(&DUPLICATE_SEQ_NAMES).clone();
            let first = sources.first().cloned().unwrap_or_default();
            let current_group = storage::load_sequence(&first)
                .ok()
                .and_then(|seq| seq.group)
                .unwrap_or_default();

            create_control(
                hwnd, hinstance, font, "STATIC", "Name:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 14, 42, 20, 0,
            );
            // One source: the user picks the copy's name. Several: each gets an automatic
            // "_copy" name, so the box is read-only and just says so.
            let (name_text, name_editable) = if sources.len() == 1 {
                (storage::unique_copy_name(&first), true)
            } else {
                (format!("(auto: {}_copy \u{2026})", first), false)
            };
            let h_name = create_edit(hwnd, hinstance, font, dpi, 10, &name_text, IDC_EDIT_SEQ_NAME);
            if !name_editable {
                EnableWindow(h_name, 0);
            }

            create_control(
                hwnd, hinstance, font, "STATIC", "Group:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 44, 42, 20, 0,
            );
            let h_group = create_edit(hwnd, hinstance, font, dpi, 40, &current_group, IDC_EDIT_DUP_GROUP);

            let hint = if sources.len() == 1 {
                "Copies keep events but no hotkey. Leave group empty for no group.".to_string()
            } else {
                format!("Copies {} sequences (events only, no hotkey). Leave group empty for no group.", sources.len())
            };
            create_control(
                hwnd, hinstance, font, "STATIC", &hint,
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 70, 300, 30, 0,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                120, 102, 70, 28, IDC_BTN_SAVE_OK,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                200, 102, 70, 28, IDC_BTN_SAVE_CANCEL,
            );

            // Focus the field the user most likely wants to change.
            let h_focus = if name_editable { h_name } else { h_group };
            SendMessageW(h_focus, EM_SETSEL as u32, 0, -1isize as LPARAM);
            SetFocus(h_focus);

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_SAVE_OK {
                let read_edit = |id: u16| {
                    let h_edit = GetDlgItem(hwnd, id as i32);
                    let mut buf = [0u16; 128];
                    let len = GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
                    String::from_utf16_lossy(&buf[..len as usize]).trim().to_string()
                };
                let group_text = read_edit(IDC_EDIT_DUP_GROUP);
                let group = if group_text.is_empty() { None } else { Some(group_text) };

                let sources = lock_or_recover(&DUPLICATE_SEQ_NAMES).clone();
                if sources.len() == 1 {
                    let new_name = read_edit(IDC_EDIT_SEQ_NAME);
                    if new_name.is_empty() {
                        MessageBoxW(hwnd, wide("Please enter a name.").as_ptr(),
                            wide("Error").as_ptr(), MB_OK | MB_ICONERROR);
                        return 0;
                    }
                    match storage::duplicate_sequence(&sources[0], &new_name, group) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            let msg = wide(&format!(
                                "A sequence named \"{}\" already exists. Choose a different name.",
                                new_name
                            ));
                            MessageBoxW(hwnd, msg.as_ptr(), wide("Name Conflict").as_ptr(),
                                MB_OK | MB_ICONWARNING);
                            return 0;
                        }
                        Err(e) => {
                            let msg = wide(&format!("Failed to duplicate: {}", e));
                            MessageBoxW(hwnd, msg.as_ptr(), wide("Error").as_ptr(), MB_OK | MB_ICONERROR);
                        }
                    }
                } else {
                    for src in &sources {
                        let new_name = storage::unique_copy_name(src);
                        if let Err(e) = storage::duplicate_sequence(src, &new_name, group.clone()) {
                            eprintln!("[Cadence] Failed to duplicate {}: {}", src, e);
                        }
                    }
                }
                if !sources.is_empty() {
                    sequences::refresh_sequences_list();
                }
                close(hwnd);
            } else if control_id == IDC_BTN_SAVE_CANCEL {
                close(hwnd);
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
