use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, remote_vk_name};
use crate::{config, hotkeys, network};
use super::*;
use super::toolbar::ToolbarControls;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static REMOTE_HWND: AtomicIsize = AtomicIsize::new(0);

// Result from the last send operation (polled by timer)
static SEND_RESULT: Mutex<Option<String>> = Mutex::new(None);

pub unsafe fn show_remote_dialog(parent: HWND) {
    let existing = REMOTE_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);

    let sx = parent_rect.left;
    let sy = parent_rect.bottom + 4;

    let hwnd = register_and_create_dialog(
        "CadenceRemote", "Remote Control",
        remote_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 360, 530,
        parent, hinstance,
    );
    REMOTE_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn remote_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = scaled_font(dpi_for_window(hwnd));

            // Load config from parent toolbar
            let parent = GetParent(hwnd);
            let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
            let cfg = if !ptr.is_null() {
                (*ptr).config.clone()
            } else {
                config::load_config()
            };

            // ---- Receiver section ----
            create_control(
                hwnd, hinstance, font, "STATIC", "-- Receiver --",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 8, 320, 16, 0,
            );

            // Port
            create_control(
                hwnd, hinstance, font, "STATIC", "Port:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 30, 32, 20, 0,
            );
            let h_port = create_control(
                hwnd, hinstance, font, "EDIT", &cfg.remote_port.to_string(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                46, 28, 56, 22, IDC_EDIT_RECV_PORT,
            );
            SendMessageW(h_port, EM_SETLIMITTEXT as u32, 5, 0);

            // Password
            create_control(
                hwnd, hinstance, font, "STATIC", "Password:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                112, 30, 56, 20, 0,
            );
            let h_pw = create_control(
                hwnd, hinstance, font, "EDIT", &cfg.remote_password,
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_PASSWORD as u32, 0,
                170, 28, 80, 22, IDC_EDIT_RECV_PASSWORD,
            );
            SendMessageW(h_pw, EM_SETLIMITTEXT as u32, 64, 0);

            // Auto-listen checkbox
            let h_auto = create_control(
                hwnd, hinstance, font, "BUTTON", "Auto",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                258, 28, 50, 22, IDC_CHK_AUTO_LISTEN,
            );
            if cfg.remote_auto_listen {
                SendMessageW(h_auto, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
            }

            // Start/Stop button
            let btn_text = if network::is_listening() { "Stop Listening" } else { "Start Listening" };
            create_control(
                hwnd, hinstance, font, "BUTTON", btn_text,
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                12, 56, 110, 26, IDC_BTN_RECV_TOGGLE,
            );

            // Receiver status
            let status_text = if network::is_listening() { "Listening" } else { "Idle" };
            create_control(
                hwnd, hinstance, font, "STATIC", status_text,
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                130, 60, 200, 18, IDC_STATIC_RECV_STATUS,
            );

            // ---- Sender section ----
            create_control(
                hwnd, hinstance, font, "STATIC", "-- Sender --",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 92, 320, 16, 0,
            );

            // Hosts label
            create_control(
                hwnd, hinstance, font, "STATIC", "Hosts:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 114, 40, 20, 0,
            );

            // Host listbox
            let h_hosts = create_control(
                hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32,
                WS_EX_CLIENTEDGE as u32,
                12, 132, 330, 70, IDC_LIST_SEND_HOSTS,
            );
            for host in &cfg.remote_hosts {
                let whost = wide(host);
                SendMessageW(h_hosts, LB_ADDSTRING, 0, whost.as_ptr() as LPARAM);
            }

            // Add host input + buttons
            let h_add_host = create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER, 0,
                12, 206, 220, 22, IDC_EDIT_ADD_HOST,
            );
            SendMessageW(h_add_host, EM_SETLIMITTEXT as u32, 64, 0);

            create_control(
                hwnd, hinstance, font, "BUTTON", "+",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                236, 206, 28, 22, IDC_BTN_ADD_HOST,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "-",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                268, 206, 28, 22, IDC_BTN_REMOVE_HOST,
            );

            // Send port
            create_control(
                hwnd, hinstance, font, "STATIC", "Port:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 236, 32, 20, 0,
            );
            let h_sport = create_control(
                hwnd, hinstance, font, "EDIT", &cfg.remote_port.to_string(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                46, 234, 56, 22, IDC_EDIT_SEND_PORT,
            );
            SendMessageW(h_sport, EM_SETLIMITTEXT as u32, 5, 0);

            // Send password
            create_control(
                hwnd, hinstance, font, "STATIC", "Pw:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                112, 236, 22, 20, 0,
            );
            let h_spw = create_control(
                hwnd, hinstance, font, "EDIT", &cfg.remote_password,
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_PASSWORD as u32, 0,
                136, 234, 80, 22, IDC_EDIT_SEND_PASSWORD,
            );
            SendMessageW(h_spw, EM_SETLIMITTEXT as u32, 64, 0);

            // Code input
            create_control(
                hwnd, hinstance, font, "STATIC", "Code:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 264, 34, 20, 0,
            );
            let h_code = create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER, 0,
                46, 262, 296, 22, IDC_EDIT_SEND_CODE,
            );
            SendMessageW(h_code, EM_SETLIMITTEXT as u32, 128, 0);

            // Send buttons
            create_control(
                hwnd, hinstance, font, "BUTTON", "Send Play",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                12, 292, 80, 28, IDC_BTN_SEND_PLAY,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Send Queue",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                100, 292, 90, 28, IDC_BTN_SEND_QUEUE,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Send Stop",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                198, 292, 80, 28, IDC_BTN_SEND_STOP,
            );

            // Sender status
            create_control(
                hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 326, 330, 18, IDC_STATIC_SEND_STATUS,
            );

            // ---- Remote Hotkeys section ----
            create_control(
                hwnd, hinstance, font, "STATIC", "-- Remote Hotkeys --",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 350, 320, 16, 0,
            );

            create_control(
                hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32,
                WS_EX_CLIENTEDGE as u32,
                12, 368, 330, 90, IDC_LIST_REMOTE_BINDINGS,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Add",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                12, 462, 60, 26, IDC_BTN_ADD_BINDING,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Remove",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                80, 462, 70, 26, IDC_BTN_REMOVE_BINDING,
            );

            populate_bindings_list(hwnd, &cfg);

            // Start polling timer
            SetTimer(hwnd, TIMER_REMOTE, 500, None);

            0
        }
        WM_TIMER => {
            if w_param == TIMER_REMOTE {
                // Update receiver status
                let h_btn = GetDlgItem(hwnd, IDC_BTN_RECV_TOGGLE as i32);
                let h_status = GetDlgItem(hwnd, IDC_STATIC_RECV_STATUS as i32);
                if network::is_listening() {
                    set_window_text(h_btn, "Stop Listening");
                    set_window_text(h_status, "Listening");
                } else {
                    set_window_text(h_btn, "Start Listening");
                    if let Some(err) = network::take_listener_error() {
                        set_window_text(h_status, &format!("Error: {}", err));
                    } else {
                        set_window_text(h_status, "Idle");
                    }
                }

                // Check for send result
                let result = lock_or_recover(&SEND_RESULT).take();
                if let Some(msg) = result {
                    let h_send_status = GetDlgItem(hwnd, IDC_STATIC_SEND_STATUS as i32);
                    set_window_text(h_send_status, &msg);
                }
            }
            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            match control_id {
                x if x == IDC_BTN_RECV_TOGGLE => handle_recv_toggle(hwnd),
                x if x == IDC_CHK_AUTO_LISTEN => handle_auto_listen_toggle(hwnd),
                x if x == IDC_BTN_ADD_HOST => handle_add_host(hwnd),
                x if x == IDC_BTN_REMOVE_HOST => handle_remove_host(hwnd),
                x if x == IDC_BTN_SEND_PLAY => handle_send_play(hwnd),
                x if x == IDC_BTN_SEND_QUEUE => handle_send_queue(hwnd),
                x if x == IDC_BTN_SEND_STOP => handle_send_stop(hwnd),
                x if x == IDC_BTN_ADD_BINDING => {
                    add_binding::show_add_binding_dialog(hwnd);
                }
                x if x == IDC_BTN_REMOVE_BINDING => handle_remove_binding(hwnd),
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            KillTimer(hwnd, TIMER_REMOTE);
            DestroyWindow(hwnd);
            REMOTE_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

// ---- Command handlers ----

unsafe fn handle_recv_toggle(hwnd: HWND) {
    if network::is_listening() {
        network::stop_listener();
    } else {
        let port = get_edit_text_u16(hwnd, IDC_EDIT_RECV_PORT).unwrap_or(9847);
        let password = get_edit_text(hwnd, IDC_EDIT_RECV_PASSWORD);
        let pw = if password.is_empty() { None } else { Some(password.clone()) };

        match network::start_listener(port, pw) {
            Ok(()) => {
                let h_status = GetDlgItem(hwnd, IDC_STATIC_RECV_STATUS as i32);
                set_window_text(h_status, &format!("Listening on port {}", port));

                // Save to config
                save_remote_config(hwnd, |cfg| {
                    cfg.remote_port = port;
                    cfg.remote_password = password;
                });
            }
            Err(e) => {
                let h_status = GetDlgItem(hwnd, IDC_STATIC_RECV_STATUS as i32);
                set_window_text(h_status, &format!("Error: {}", e));
            }
        }
    }
}

unsafe fn handle_auto_listen_toggle(hwnd: HWND) {
    let h_chk = GetDlgItem(hwnd, IDC_CHK_AUTO_LISTEN as i32);
    let checked = SendMessageW(h_chk, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
    save_remote_config(hwnd, |cfg| {
        cfg.remote_auto_listen = checked;
    });
}

unsafe fn handle_send_play(hwnd: HWND) {
    let code = get_edit_text(hwnd, IDC_EDIT_SEND_CODE);
    if code.is_empty() {
        let h_status = GetDlgItem(hwnd, IDC_STATIC_SEND_STATUS as i32);
        set_window_text(h_status, "Enter a code (sequence name)");
        return;
    }
    let command = format!("PLAY {}", code);
    do_send(hwnd, &command);
}

unsafe fn handle_send_queue(hwnd: HWND) {
    do_send(hwnd, "PLAY_QUEUE");
}

unsafe fn handle_send_stop(hwnd: HWND) {
    do_send(hwnd, "STOP");
}

unsafe fn handle_add_host(hwnd: HWND) {
    let host = get_edit_text(hwnd, IDC_EDIT_ADD_HOST);
    if host.is_empty() {
        return;
    }
    // Add to listbox
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEND_HOSTS as i32);
    let whost = wide(&host);
    SendMessageW(h_list, LB_ADDSTRING, 0, whost.as_ptr() as LPARAM);
    // Clear the input
    set_window_text(GetDlgItem(hwnd, IDC_EDIT_ADD_HOST as i32), "");
    // Save to config
    save_remote_config(hwnd, |cfg| {
        cfg.remote_hosts.push(host);
    });
}

unsafe fn handle_remove_host(hwnd: HWND) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEND_HOSTS as i32);
    let idx = SendMessageW(h_list, LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return;
    }
    SendMessageW(h_list, LB_DELETESTRING, idx as usize, 0);
    save_remote_config(hwnd, |cfg| {
        let i = idx as usize;
        if i < cfg.remote_hosts.len() {
            cfg.remote_hosts.remove(i);
        }
    });
}

unsafe fn do_send(hwnd: HWND, command: &str) {
    let port = get_edit_text_u16(hwnd, IDC_EDIT_SEND_PORT).unwrap_or(9847);
    let password = get_edit_text(hwnd, IDC_EDIT_SEND_PASSWORD);

    // Get hosts from config
    let parent = GetParent(hwnd);
    let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
    let hosts = if !ptr.is_null() {
        (*ptr).config.remote_hosts.clone()
    } else {
        config::load_config().remote_hosts
    };

    if hosts.is_empty() {
        let h_status = GetDlgItem(hwnd, IDC_STATIC_SEND_STATUS as i32);
        set_window_text(h_status, "Add at least one host");
        return;
    }

    let h_status = GetDlgItem(hwnd, IDC_STATIC_SEND_STATUS as i32);
    let count = hosts.len();
    set_window_text(h_status, &format!("Sending to {} host(s)...", count));

    let pw = if password.is_empty() { None } else { Some(password) };
    let cmd = command.to_string();

    // Broadcast: one thread per host
    for host in hosts {
        let cmd = cmd.clone();
        let pw = pw.clone();
        std::thread::spawn(move || {
            let result = network::send_command(&host, port, pw.as_deref(), &cmd);
            let msg = match result {
                Ok(resp) => format!("{}: OK ({})", host, resp),
                Err(e) => format!("{}: Error ({})", host, e),
            };
            *lock_or_recover(&SEND_RESULT) = Some(msg);
        });
    }
}

// ---- Helpers ----

unsafe fn get_edit_text(hwnd: HWND, control_id: u16) -> String {
    let h_edit = GetDlgItem(hwnd, control_id as i32);
    let len = GetWindowTextLengthW(h_edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    GetWindowTextW(h_edit, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..len as usize])
}

unsafe fn get_edit_text_u16(hwnd: HWND, control_id: u16) -> Option<u16> {
    get_edit_text(hwnd, control_id).parse().ok()
}

unsafe fn set_window_text(hwnd: HWND, text: &str) {
    let wtext = wide(text);
    SetWindowTextW(hwnd, wtext.as_ptr());
}

unsafe fn handle_remove_binding(hwnd: HWND) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_REMOTE_BINDINGS as i32);
    let idx = SendMessageW(h_list, LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return;
    }
    let idx = idx as usize;
    save_remote_config(hwnd, |cfg| {
        if idx < cfg.remote_bindings.len() {
            cfg.remote_bindings.remove(idx);
        }
    });
    reload_remote_bindings(hwnd);
}

/// Refresh the bindings list after add/remove. Called from add_binding dialog too.
pub(crate) unsafe fn refresh_bindings_list(hwnd: HWND) {
    // hwnd is the remote dialog
    let parent = GetParent(hwnd);
    let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
    let cfg = if !ptr.is_null() {
        (*ptr).config.clone()
    } else {
        config::load_config()
    };
    populate_bindings_list(hwnd, &cfg);
    // Also update the hook
    hotkeys::set_remote_bindings(cfg.remote_bindings.clone());
}

unsafe fn reload_remote_bindings(hwnd: HWND) {
    refresh_bindings_list(hwnd);
}

unsafe fn populate_bindings_list(hwnd: HWND, cfg: &config::AppConfig) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_REMOTE_BINDINGS as i32);
    if h_list.is_null() {
        return;
    }
    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    for b in &cfg.remote_bindings {
        let display = format_binding(b);
        let wname = wide(&display);
        SendMessageW(h_list, LB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
    }
}

fn format_binding(b: &crate::sequence::RemoteBinding) -> String {
    use crate::sequence::BindingTarget;
    let mut parts = Vec::new();
    if b.modifiers & hotkeys::MOD_FLAG_CTRL != 0 {
        parts.push("Ctrl");
    }
    if b.modifiers & hotkeys::MOD_FLAG_ALT != 0 {
        parts.push("Alt");
    }
    if b.modifiers & hotkeys::MOD_FLAG_SHIFT != 0 {
        parts.push("Shift");
    }
    parts.push(remote_vk_name(b.vk_code));
    let kind = match b.target {
        BindingTarget::Sequence => "",
        BindingTarget::Queue => "queue: ",
        BindingTarget::Group => "group: ",
    };
    format!("{} \u{2192} {}{}", parts.join("+"), kind, b.sequence_name)
}

unsafe fn save_remote_config<F: FnOnce(&mut config::AppConfig)>(hwnd: HWND, updater: F) {
    let parent = GetParent(hwnd);
    let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
    if !ptr.is_null() {
        updater(&mut (*ptr).config);
        if let Err(e) = config::save_config(&(*ptr).config) {
            eprintln!("[Cadence] Config save failed: {}", e);
        }
    }
}
