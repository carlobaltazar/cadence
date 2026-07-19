use crate::win32_helpers::{wide, create_control, register_and_create_dialog, populate_key_combo, dpi_for_window, scaled_font, REMOTE_KEY_OPTIONS};
use crate::{config, hotkeys};
use crate::sequence::RemoteBinding;
use super::*;
use super::toolbar::ToolbarControls;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static ADD_BINDING_HWND: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn show_add_binding_dialog(parent: HWND) {
    let existing = ADD_BINDING_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);

    let sx = parent_rect.left + 20;
    let sy = parent_rect.top + 20;

    let hwnd = register_and_create_dialog(
        "CadenceAddBinding", "Add Remote Hotkey",
        add_binding_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 300, 200,
        parent, hinstance,
    );
    ADD_BINDING_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn add_binding_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = scaled_font(dpi_for_window(hwnd));

            // Modifiers label
            create_control(
                hwnd, hinstance, font, "STATIC", "Modifiers:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 14, 64, 20, 0,
            );

            // Modifier checkboxes
            create_control(
                hwnd, hinstance, font, "BUTTON", "Alt",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                80, 12, 46, 22, IDC_CHK_MOD_ALT,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Ctrl",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                130, 12, 50, 22, IDC_CHK_MOD_CTRL,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Shift",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                184, 12, 54, 22, IDC_CHK_MOD_SHIFT,
            );

            // Key label + combo
            create_control(
                hwnd, hinstance, font, "STATIC", "Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 46, 64, 20, 0,
            );

            let h_combo = create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                80, 42, 120, 300, IDC_COMBO_BIND_VK,
            );
            populate_key_combo(h_combo, REMOTE_KEY_OPTIONS, None);

            // Sequence label + edit
            create_control(
                hwnd, hinstance, font, "STATIC", "Sequence:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 80, 64, 20, 0,
            );

            let h_seq = create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER, 0,
                80, 78, 196, 22, IDC_EDIT_BIND_SEQ,
            );
            SendMessageW(h_seq, EM_SETLIMITTEXT as u32, 128, 0);

            // OK / Cancel buttons
            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                80, 120, 60, 28, IDC_BTN_BIND_ADD_OK,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                150, 120, 60, 28, IDC_BTN_BIND_ADD_CANCEL,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            match control_id {
                x if x == IDC_BTN_BIND_ADD_OK => {
                    handle_ok(hwnd);
                }
                x if x == IDC_BTN_BIND_ADD_CANCEL => {
                    DestroyWindow(hwnd);
                    ADD_BINDING_HWND.store(0, Ordering::Release);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            ADD_BINDING_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

unsafe fn handle_ok(hwnd: HWND) {
    // Read modifiers
    let mut modifiers: u32 = 0;
    if SendMessageW(GetDlgItem(hwnd, IDC_CHK_MOD_ALT as i32), BM_GETCHECK, 0, 0) == BST_CHECKED as isize {
        modifiers |= hotkeys::MOD_FLAG_ALT;
    }
    if SendMessageW(GetDlgItem(hwnd, IDC_CHK_MOD_CTRL as i32), BM_GETCHECK, 0, 0) == BST_CHECKED as isize {
        modifiers |= hotkeys::MOD_FLAG_CTRL;
    }
    if SendMessageW(GetDlgItem(hwnd, IDC_CHK_MOD_SHIFT as i32), BM_GETCHECK, 0, 0) == BST_CHECKED as isize {
        modifiers |= hotkeys::MOD_FLAG_SHIFT;
    }

    // Must have at least one modifier
    if modifiers == 0 {
        let msg = wide("Select at least one modifier (Alt, Ctrl, or Shift)");
        let title = wide("Error");
        MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        return;
    }

    // Read key
    let h_combo = GetDlgItem(hwnd, IDC_COMBO_BIND_VK as i32);
    let key_idx = SendMessageW(h_combo, CB_GETCURSEL, 0, 0) as usize;
    if key_idx >= REMOTE_KEY_OPTIONS.len() {
        let msg = wide("Select a key");
        let title = wide("Error");
        MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        return;
    }
    let vk_code = REMOTE_KEY_OPTIONS[key_idx].0;

    // Read sequence name
    let h_seq = GetDlgItem(hwnd, IDC_EDIT_BIND_SEQ as i32);
    let len = GetWindowTextLengthW(h_seq);
    if len <= 0 {
        let msg = wide("Enter a sequence name");
        let title = wide("Error");
        MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        return;
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    GetWindowTextW(h_seq, buf.as_mut_ptr(), buf.len() as i32);
    let sequence_name = String::from_utf16_lossy(&buf[..len as usize]);

    let binding = RemoteBinding {
        modifiers,
        vk_code,
        sequence_name,
    };

    // Save to config via the remote dialog's parent (toolbar)
    let remote_hwnd = GetParent(hwnd); // remote dialog
    let toolbar_hwnd = GetParent(remote_hwnd); // toolbar
    let ptr = GetWindowLongPtrW(toolbar_hwnd, GWLP_USERDATA) as *mut ToolbarControls;
    if !ptr.is_null() {
        (*ptr).config.remote_bindings.push(binding);
        if let Err(e) = config::save_config(&(*ptr).config) {
            eprintln!("[Cadence] Config save failed: {}", e);
        }
    }

    // Refresh the bindings list in the remote dialog
    if !remote_hwnd.is_null() && IsWindow(remote_hwnd) != 0 {
        remote::refresh_bindings_list(remote_hwnd);
    }

    // Close this dialog
    DestroyWindow(hwnd);
    ADD_BINDING_HWND.store(0, Ordering::Release);
}
