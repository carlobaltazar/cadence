use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, KEY_OPTIONS};
use crate::{hotkeys, storage};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static BIND_KEY_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_bind_key_dialog(parent: HWND) {
    let existing = BIND_KEY_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);

    let sx = parent_rect.left + 50;
    let sy = parent_rect.top + 100;

    let hwnd = register_and_create_dialog(
        "CadenceBindKey", "Bind Key",
        bind_key_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 230, 115,
        parent, hinstance,
    );
    BIND_KEY_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn bind_key_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = scaled_font(dpi_for_window(hwnd));

            // "Key:" label
            create_control(
                hwnd, hinstance, font, "STATIC", "Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 14, 34, 20, 0,
            );

            // Key combobox
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                48, 10, 164, 200, IDC_COMBO_BIND_KEY,
            );

            let h_combo = GetDlgItem(hwnd, IDC_COMBO_BIND_KEY as i32);

            // Populate with available keys
            // Add "(None)" option first
            let none_text = wide("(None)");
            SendMessageW(h_combo, CB_ADDSTRING, 0, none_text.as_ptr() as LPARAM);

            // Get current binding for this sequence
            let current_vk = {
                let name_guard = lock_or_recover(&BIND_SEQ_NAME);
                if let Some(ref name) = *name_guard {
                    storage::load_sequence(name)
                        .ok()
                        .and_then(|s| s.hotkey.map(|h| h.vk_code))
                } else {
                    None
                }
            };

            let available = available_keys_for_binding();
            let mut selected_idx: usize = 0; // default to "(None)"
            for (vk, key_name) in &available {
                let wname = wide(key_name);
                SendMessageW(h_combo, CB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
                if Some(*vk) == current_vk {
                    // +1 because "(None)" is at index 0
                    let count = SendMessageW(h_combo, CB_GETCOUNT, 0, 0) as usize;
                    selected_idx = count - 1;
                }
            }
            SendMessageW(h_combo, CB_SETCURSEL, selected_idx, 0);

            // OK button
            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                42, 52, 66, 28, IDC_BTN_BIND_OK,
            );

            // Cancel button
            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                118, 52, 66, 28, IDC_BTN_BIND_CANCEL,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_BIND_OK {
                let h_combo = GetDlgItem(hwnd, IDC_COMBO_BIND_KEY as i32);
                let idx = SendMessageW(h_combo, CB_GETCURSEL, 0, 0) as usize;

                let seq_name = lock_or_recover(&BIND_SEQ_NAME).clone();
                if let Some(name) = seq_name {
                    if let Ok(mut seq) = storage::load_sequence(&name) {
                        if idx == 0 {
                            // "(None)" selected - clear binding
                            seq.clear_hotkey();
                        } else {
                            // Get the VK from available keys list
                            let available = available_keys_for_binding();
                            if idx - 1 < available.len() {
                                let (vk, _) = available[idx - 1];
                                seq.set_hotkey(vk);
                            }
                        }
                        if let Err(e) = storage::save_sequence(&seq) {
                            eprintln!("[Cadence] Failed to save sequence binding: {}", e);
                        }
                        refresh_bindings();
                        sequences::refresh_sequences_list();
                    }
                }

                DestroyWindow(hwnd);
                BIND_KEY_HWND.store(0, Ordering::Release);
            } else if control_id == IDC_BTN_BIND_CANCEL {
                DestroyWindow(hwnd);
                BIND_KEY_HWND.store(0, Ordering::Release);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            BIND_KEY_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

/// Get keys available for binding (excluding record_vk, stop_vk, and already-bound keys).
fn available_keys_for_binding() -> Vec<(u16, &'static str)> {
    let (record_vk, stop_vk) = hotkeys::current_hotkeys();
    let bindings = hotkeys::current_sequence_bindings();
    let editing_name = lock_or_recover(&BIND_SEQ_NAME).clone();

    KEY_OPTIONS
        .iter()
        .filter(|(vk, _)| {
            *vk != record_vk
                && *vk != stop_vk
                && !bindings.iter().any(|(bvk, name)| {
                    *bvk == *vk && editing_name.as_ref().map_or(true, |en| name != en)
                })
        })
        .map(|(vk, name)| (*vk, *name))
        .collect()
}
