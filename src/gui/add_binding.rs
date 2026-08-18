use crate::win32_helpers::{wide, create_control, register_and_create_dialog, populate_key_combo, dpi_for_window, scaled_font, REMOTE_KEY_OPTIONS};
use crate::{config, hotkeys, storage};
use crate::sequence::{BindingTarget, RemoteBinding};
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
        sx, sy, 300, 236,
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

            // Target kind: what the hotkey fires on the hosts. A saved queue / group is
            // expanded into sequence names by THIS machine when the key is pressed.
            create_control(
                hwnd, hinstance, font, "STATIC", "Target:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 80, 64, 20, 0,
            );
            let h_kind = create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                80, 76, 120, 200, IDC_COMBO_BIND_KIND,
            );
            for kind in BindingTarget::ALL {
                SendMessageW(h_kind, CB_ADDSTRING, 0, wide(kind.label()).as_ptr() as LPARAM);
            }
            SendMessageW(h_kind, CB_SETCURSEL, 0, 0);

            // Name: editable drop-down listing what exists of the chosen kind (typing is
            // still allowed — the sequence may only exist on the hosts).
            create_control(
                hwnd, hinstance, font, "STATIC", "Name:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 114, 64, 20, 0,
            );
            let h_name = create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWN as u32 | CBS_AUTOHSCROLL as u32 | WS_VSCROLL, 0,
                80, 110, 196, 300, IDC_COMBO_BIND_NAME,
            );
            SendMessageW(h_name, CB_LIMITTEXT, 128, 0);
            populate_names(h_name, BindingTarget::Sequence);

            // OK / Cancel buttons
            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                80, 156, 60, 28, IDC_BTN_BIND_ADD_OK,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Cancel",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                150, 156, 60, 28, IDC_BTN_BIND_ADD_CANCEL,
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
                x if x == IDC_COMBO_BIND_KIND => {
                    if HIWORD(w_param as u32) == CBN_SELCHANGE {
                        let kind = selected_kind(hwnd);
                        populate_names(GetDlgItem(hwnd, IDC_COMBO_BIND_NAME as i32), kind);
                    }
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

unsafe fn selected_kind(hwnd: HWND) -> BindingTarget {
    let idx = SendMessageW(GetDlgItem(hwnd, IDC_COMBO_BIND_KIND as i32), CB_GETCURSEL, 0, 0);
    BindingTarget::ALL.get(idx.max(0) as usize).copied().unwrap_or_default()
}

/// Fill the name drop-down with everything of `kind` on this machine (typed text is cleared).
unsafe fn populate_names(h_name: HWND, kind: BindingTarget) {
    SendMessageW(h_name, CB_RESETCONTENT, 0, 0);
    let names: Vec<String> = match kind {
        BindingTarget::Sequence => storage::list_sequences().unwrap_or_default(),
        BindingTarget::Queue => storage::list_saved_queues(),
        BindingTarget::Group => {
            let mut groups: Vec<String> = storage::list_sequence_meta()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.group)
                .collect();
            groups.sort();
            groups.dedup();
            groups
        }
    };
    for n in &names {
        SendMessageW(h_name, CB_ADDSTRING, 0, wide(n).as_ptr() as LPARAM);
    }
    SetWindowTextW(h_name, wide("").as_ptr());
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

    // Read target kind + name (GetWindowText on a CBS_DROPDOWN combo reads its edit box)
    let target = selected_kind(hwnd);
    let h_name = GetDlgItem(hwnd, IDC_COMBO_BIND_NAME as i32);
    let mut buf = [0u16; 160];
    let len = GetWindowTextW(h_name, buf.as_mut_ptr(), buf.len() as i32);
    let sequence_name = String::from_utf16_lossy(&buf[..len.max(0) as usize]).trim().to_string();
    if sequence_name.is_empty() {
        let msg = wide(&format!("Enter or pick a {} name", target.label().to_lowercase()));
        let title = wide("Error");
        MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        return;
    }

    let binding = RemoteBinding {
        modifiers,
        vk_code,
        sequence_name,
        target,
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
