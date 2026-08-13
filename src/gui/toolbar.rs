use crate::win32_helpers::{
    wide, create_control, dpi_for_window, message_box_timeout, scaled_font, scale, MB_TIMEDOUT,
};
use crate::{burst, config, monitor, network, pet_cycle, player, proximity, recorder, resume, update};
use super::*;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

pub(crate) struct ToolbarControls {
    pub hwnd_main: HWND,
    pub hwnd_btn_record: HWND,
    pub hwnd_btn_play: HWND,
    pub hwnd_chk_loop: HWND,
    pub hwnd_chk_topmost: HWND,
    pub hwnd_chk_pet: HWND,
    pub hwnd_chk_hp: HWND,
    pub hwnd_chk_mp: HWND,
    pub hwnd_chk_sp: HWND,
    pub hwnd_chk_det: HWND,
    pub hwnd_btn_burst: HWND,
    pub hwnd_status: HWND,
    pub config: config::AppConfig,
}

pub fn create_toolbar_window(cfg: &config::AppConfig) -> HWND {
    unsafe {
        let class_name = wide("CadenceMain");
        let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(toolbar_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_TOOLWINDOW
            | if cfg.always_on_top {
                WS_EX_TOPMOST
            } else {
                0
            };

        // Create hidden first so we can query the window's actual monitor DPI,
        // then scale the layout and position it top-right. Per-Monitor-V2
        // awareness means the OS won't scale us — we size against the real DPI.
        let title = wide("Cadence");
        let hwnd = CreateWindowExW(
            ex_style as u32,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            0,
            0,
            100,
            100,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );

        let dpi = dpi_for_window(hwnd);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: scale(890, dpi),
            bottom: scale(52, dpi),
        };
        AdjustWindowRectEx(&mut rect, style, FALSE, ex_style as u32);
        let actual_w = rect.right - rect.left;
        let actual_h = rect.bottom - rect.top;
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let x = screen_w - actual_w - scale(10, dpi);
        let y = scale(10, dpi);
        let z_order = if cfg.always_on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
        SetWindowPos(hwnd, z_order, x, y, actual_w, actual_h, SWP_SHOWWINDOW);

        // Store config in controls struct
        let controls = Box::new(ToolbarControls {
            hwnd_main: hwnd,
            hwnd_btn_record: std::ptr::null_mut(),
            hwnd_btn_play: std::ptr::null_mut(),
            hwnd_chk_loop: std::ptr::null_mut(),
            hwnd_chk_topmost: std::ptr::null_mut(),
            hwnd_chk_pet: std::ptr::null_mut(),
            hwnd_chk_hp: std::ptr::null_mut(),
            hwnd_chk_mp: std::ptr::null_mut(),
            hwnd_chk_sp: std::ptr::null_mut(),
            hwnd_chk_det: std::ptr::null_mut(),
            hwnd_btn_burst: std::ptr::null_mut(),
            hwnd_status: std::ptr::null_mut(),
            config: cfg.clone(),
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(controls) as isize);

        // Create child controls
        create_controls(hwnd, hinstance, cfg);

        // Start status timer
        SetTimer(hwnd, TIMER_STATUS, 200, None);

        hwnd
    }
}

unsafe fn create_controls(hwnd: HWND, hinstance: HINSTANCE, cfg: &config::AppConfig) {
    let font = scaled_font(dpi_for_window(hwnd));

    let y = 6;
    let h = 26;

    // -- Group 1: Recording controls --
    let btn_rec = create_control(
        hwnd, hinstance, font, "BUTTON", "Record",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        6, y, 56, h, IDC_BTN_RECORD,
    );

    let btn_play = create_control(
        hwnd, hinstance, font, "BUTTON", "Play",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        66, y, 50, h, IDC_BTN_PLAY,
    );

    // -- Group 2: Mode toggles (gap before) --
    let chk_loop = create_control(
        hwnd, hinstance, font, "BUTTON", "Loop",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        126, y, 54, h, IDC_CHK_LOOP,
    );
    if cfg.loop_playback {
        SendMessageW(chk_loop, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    let chk_top = create_control(
        hwnd, hinstance, font, "BUTTON", "Top",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        182, y, 46, h, IDC_CHK_TOPMOST,
    );
    if cfg.always_on_top {
        SendMessageW(chk_top, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    let chk_pet = create_control(
        hwnd, hinstance, font, "BUTTON", "Pet",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        230, y, 42, h, IDC_CHK_PET,
    );
    if cfg.pet_cycle_enabled {
        SendMessageW(chk_pet, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    let chk_hp = create_control(
        hwnd, hinstance, font, "BUTTON", "HP",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        274, y, 36, h, IDC_CHK_HP,
    );
    if cfg.hp_monitor_enabled {
        SendMessageW(chk_hp, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    let chk_mp = create_control(
        hwnd, hinstance, font, "BUTTON", "MP",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        312, y, 36, h, IDC_CHK_MP,
    );
    if cfg.mp_monitor_enabled {
        SendMessageW(chk_mp, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    let chk_sp = create_control(
        hwnd, hinstance, font, "BUTTON", "SP",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        350, y, 36, h, IDC_CHK_SP,
    );
    if cfg.sp_monitor_enabled {
        SendMessageW(chk_sp, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    // Det — arm proximity detection. One-shot: it unchecks itself when a player is detected.
    let chk_det = create_control(
        hwnd, hinstance, font, "BUTTON", "Det",
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
        390, y, 42, h, IDC_CHK_DET,
    );
    if cfg.proximity_enabled {
        SendMessageW(chk_det, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
    }

    // Burst Q button — toggles rapid Q-press mode. Owner-drawn-ish:
    // updates label/state in WM_TIMER once burst::is_active changes.
    let btn_burst = create_control(
        hwnd, hinstance, font, "BUTTON", "Burst Q",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        438, y, 62, h, IDC_BTN_BURST,
    );

    // -- Group 3: Navigation (gap before) --
    create_control(
        hwnd, hinstance, font, "BUTTON", "\u{2699}",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        504, y, 26, h, IDC_BTN_SETTINGS,
    );

    create_control(
        hwnd, hinstance, font, "BUTTON", "Sequences",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        534, y, 74, h, IDC_BTN_SEQUENCES,
    );

    create_control(
        hwnd, hinstance, font, "BUTTON", "Remote",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
        614, y, 66, h, IDC_BTN_REMOTE,
    );

    // -- Status label -- (reclaims the space the removed Wx checkbox used to occupy)
    let status = create_control(
        hwnd, hinstance, font, "STATIC", "Idle",
        WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
        684, y + 2, 200, h, IDC_STATUS,
    );

    // Update stored controls
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
    if !ptr.is_null() {
        (*ptr).hwnd_btn_record = btn_rec;
        (*ptr).hwnd_btn_play = btn_play;
        (*ptr).hwnd_chk_loop = chk_loop;
        (*ptr).hwnd_chk_topmost = chk_top;
        (*ptr).hwnd_chk_pet = chk_pet;
        (*ptr).hwnd_chk_hp = chk_hp;
        (*ptr).hwnd_chk_mp = chk_mp;
        (*ptr).hwnd_chk_sp = chk_sp;
        (*ptr).hwnd_chk_det = chk_det;
        (*ptr).hwnd_btn_burst = btn_burst;
        (*ptr).hwnd_status = status;
    }
}

/// Show the "new version available" prompt and carry out the answer. Must run on the UI thread —
/// the "skip this version" branch writes the toolbar's config. Both the periodic check and the
/// Settings button reach it the same way, by posting WM_APP_UPDATE_AVAILABLE here.
///
/// The prompt auto-answers Yes after `update_auto_confirm_secs` (0 = wait forever), so the 22
/// unattended fleet VMs converge on a release by themselves while a human at the console can
/// still answer No/Cancel. Auto-Yes on a farming VM stops playback first and stages a resume
/// marker so the updated instance picks the same sequence/queue straight back up.
pub(crate) unsafe fn offer_update(toolbar: HWND, info: update::UpdateInfo) {
    let owner = toolbar;
    let cfg = config::cached_config();
    let timeout_ms = cfg.update_auto_confirm_secs.saturating_mul(1000);
    // Long release notes would push the buttons off screen; a summary is enough to decide.
    let mut notes: String = info.notes.lines().take(8).collect::<Vec<_>>().join("\n");
    if notes.chars().count() > 600 {
        notes = notes.chars().take(600).collect::<String>() + "\u{2026}";
    }
    let head = format!(
        "Cadence {} is available.\nYou have {}.",
        info.version,
        update::current_version()
    );

    // A copy living somewhere unwritable (e.g. Program Files) can't swap itself out. Don't try to
    // elevate — just offer the download page. A timeout counts as No: never pop a browser open
    // on an unattended VM.
    if !update::can_self_update() {
        let text = format!(
            "{}\n\n{}\n\nThis copy is in a folder Cadence can't write to, so it can't update \
             itself.\n\nOpen the download page?",
            head, notes
        );
        if message_box_timeout(owner, &text, "Cadence Update",
            MB_YESNO | MB_ICONINFORMATION, timeout_ms) == IDYES
        {
            open_url(update::RELEASES_PAGE);
        }
        update::note_declined(&info.version);
        update::set_prompt_active(false);
        return;
    }

    let auto_line = if timeout_ms != 0 {
        format!("\n\nAuto-installs in {} s if unattended.", cfg.update_auto_confirm_secs)
    } else {
        String::new()
    };
    let text = format!(
        "{}\n\n{}\n\nYes \u{2014} download and restart now\nNo \u{2014} remind me later\n\
         Cancel \u{2014} skip version {}{}",
        head, notes, info.version, auto_line
    );
    let choice = message_box_timeout(owner, &text, "Cadence Update",
        MB_YESNOCANCEL | MB_ICONINFORMATION, timeout_ms);
    match choice {
        c if c == IDYES || c == MB_TIMEDOUT => {
            // Hint that something is happening; the status label is rewritten every timer tick,
            // so the window title is the one spot that stays put during the download.
            SetWindowTextW(toolbar, wide("Cadence \u{2014} updating\u{2026}").as_ptr());
            let tb = toolbar as isize;
            std::thread::spawn(move || {
                // Stop the farm at an event boundary and remember what it was doing, so the
                // updated instance can pick it straight back up. Proceed even if the playback
                // thread is stuck in a long delay — it dies with this process anyway.
                let source = player::current_source();
                let was_playing = player::stop_and_wait(15_000);
                if was_playing && config::cached_config().update_auto_resume {
                    resume::write_marker(&source);
                }
                match update::apply(&info) {
                    // The replacement is already running: close this instance so it releases
                    // the exe.
                    Ok(()) => {
                        PostMessageW(tb as HWND, WM_CLOSE, 0, 0);
                    }
                    Err(e) => {
                        // The restart the marker was staged for isn't happening; restart the
                        // playback we stopped so the farm keeps running, and let the next
                        // periodic poll retry the update.
                        resume::clear_marker();
                        if was_playing {
                            resume::start_from(&source);
                        }
                        update::set_prompt_active(false);
                        // SetWindowTextW (not a posted WM_SETTEXT) so the string outlives
                        // the call.
                        SetWindowTextW(tb as HWND, wide("Cadence").as_ptr());
                        let msg = format!("Update failed:\n\n{}\n\nCadence is unchanged.", e);
                        // Timed too — an error box must not sit forever on an unattended VM.
                        message_box_timeout(std::ptr::null_mut(), &msg, "Cadence Update",
                            MB_OK | MB_ICONERROR, 60_000);
                    }
                }
            });
        }
        IDCANCEL => {
            let ptr = GetWindowLongPtrW(toolbar, GWLP_USERDATA) as *mut ToolbarControls;
            if !ptr.is_null() {
                (*ptr).config.update_skip_version = info.version.clone();
                if let Err(e) = config::save_config(&(*ptr).config) {
                    eprintln!("[Cadence] Config save failed: {}", e);
                }
            }
            update::set_prompt_active(false);
        }
        _ => {
            // IDNO: snooze this version for a while (and ask again next launch regardless).
            update::note_declined(&info.version);
            update::set_prompt_active(false);
        }
    }
}

/// Open a URL in the default browser.
pub(crate) unsafe fn open_url(url: &str) {
    winapi::um::shellapi::ShellExecuteW(
        std::ptr::null_mut(),
        wide("open").as_ptr(),
        wide(url).as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn warn_unconfigured(hwnd: HWND, bar: &str) {
    let msg = crate::win32_helpers::wide(&format!(
        "Configure {} pixel in Settings first.\nSet X/Y and click Sample (or Pick).",
        bar
    ));
    let title = crate::win32_helpers::wide(&format!("{} Monitor", bar));
    MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING);
}

unsafe extern "system" fn toolbar_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            match control_id {
                x if x == IDC_BTN_RECORD => handle_record_toggle(),
                x if x == IDC_BTN_PLAY => handle_play_toggle(),
                x if x == IDC_CHK_LOOP => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_loop, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        player::set_loop_mode(checked);
                        (*ptr).config.loop_playback = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_TOPMOST => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_topmost, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        let z_order = if checked {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        };
                        SetWindowPos(hwnd, z_order, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                        (*ptr).config.always_on_top = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_HP => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_hp, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        if checked {
                            let cfg = &(*ptr).config;
                            if cfg.hp_monitor_color != 0 {
                                monitor::set_bar(
                                    monitor::Bar::Hp,
                                    cfg.hp_monitor_window_class.clone(),
                                    cfg.hp_monitor_window_title.clone(),
                                    cfg.hp_monitor_x,
                                    cfg.hp_monitor_y,
                                    cfg.hp_monitor_color,
                                );
                            } else {
                                warn_unconfigured(hwnd, "HP");
                                SendMessageW((*ptr).hwnd_chk_hp, BM_SETCHECK, BST_UNCHECKED as WPARAM, 0);
                                return 0;
                            }
                        } else {
                            monitor::disable_bar(monitor::Bar::Hp);
                        }
                        (*ptr).config.hp_monitor_enabled = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_MP => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_mp, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        if checked {
                            let cfg = &(*ptr).config;
                            if cfg.mp_monitor_color != 0 {
                                monitor::set_bar(
                                    monitor::Bar::Mp,
                                    cfg.hp_monitor_window_class.clone(),
                                    cfg.hp_monitor_window_title.clone(),
                                    cfg.mp_monitor_x,
                                    cfg.mp_monitor_y,
                                    cfg.mp_monitor_color,
                                );
                            } else {
                                warn_unconfigured(hwnd, "MP");
                                SendMessageW((*ptr).hwnd_chk_mp, BM_SETCHECK, BST_UNCHECKED as WPARAM, 0);
                                return 0;
                            }
                        } else {
                            monitor::disable_bar(monitor::Bar::Mp);
                        }
                        (*ptr).config.mp_monitor_enabled = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_SP => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_sp, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        if checked {
                            let cfg = &(*ptr).config;
                            if cfg.sp_monitor_color != 0 {
                                monitor::set_bar(
                                    monitor::Bar::Sp,
                                    cfg.hp_monitor_window_class.clone(),
                                    cfg.hp_monitor_window_title.clone(),
                                    cfg.sp_monitor_x,
                                    cfg.sp_monitor_y,
                                    cfg.sp_monitor_color,
                                );
                            } else {
                                warn_unconfigured(hwnd, "SP");
                                SendMessageW((*ptr).hwnd_chk_sp, BM_SETCHECK, BST_UNCHECKED as WPARAM, 0);
                                return 0;
                            }
                        } else {
                            monitor::disable_bar(monitor::Bar::Sp);
                        }
                        (*ptr).config.sp_monitor_enabled = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_PET => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_pet, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        if checked {
                            pet_cycle::start((*ptr).config.pet_cycle_interval_secs);
                        } else {
                            pet_cycle::stop();
                        }
                        (*ptr).config.pet_cycle_enabled = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_CHK_DET => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let checked = SendMessageW((*ptr).hwnd_chk_det, BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        if checked {
                            let cfg = &(*ptr).config;
                            proximity::set_ignored(cfg.proximity_ignore.clone());
                            proximity::set_reaction(cfg.proximity_sequence.clone());
                            proximity::set_trigger_watch(cfg.proximity_watch_only);
                            proximity::set_watch_list(cfg.proximity_watch.clone());
                            proximity::set_watch_gm_flag(cfg.proximity_watch_gm_flag);
                            proximity::set_scan_only(false); // Det always reacts (one-shot)
                            proximity::start(
                                cfg.proximity_vk,
                                cfg.proximity_iface.clone(),
                                cfg.proximity_server_ip.clone(),
                                cfg.proximity_cooldown_ms,
                            );
                        } else {
                            proximity::stop();
                        }
                        (*ptr).config.proximity_enabled = checked;
                        if let Err(e) = config::save_config(&(*ptr).config) {
                            eprintln!("[Cadence] Config save failed: {}", e);
                        }
                    }
                }
                x if x == IDC_BTN_BURST => {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                    if !ptr.is_null() {
                        let cfg = &(*ptr).config;
                        burst::toggle(
                            cfg.burst_rate_hz,
                            cfg.hp_monitor_window_class.clone(),
                            cfg.hp_monitor_window_title.clone(),
                        );
                    }
                }
                x if x == IDC_BTN_SETTINGS => {
                    settings::show_settings_dialog(hwnd);
                }
                x if x == IDC_BTN_SEQUENCES => {
                    sequences::show_sequences_window(hwnd);
                }
                x if x == IDC_BTN_REMOTE => {
                    remote::show_remote_dialog(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_APP_PROXIMITY_HIT => {
            // Proximity fired and disarmed itself (one-shot): reflect it by unchecking "Det".
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
            if !ptr.is_null() {
                SendMessageW((*ptr).hwnd_chk_det, BM_SETCHECK, BST_UNCHECKED as WPARAM, 0);
                (*ptr).config.proximity_enabled = false;
                if let Err(e) = config::save_config(&(*ptr).config) {
                    eprintln!("[Cadence] Config save failed: {}", e);
                }
            }
            0
        }
        WM_APP_UPDATE_AVAILABLE => {
            // The background check found a newer release; prompt now that we're on the UI thread.
            if let Some(info) = update::take_pending() {
                offer_update(hwnd, info);
            } else {
                // Nothing staged (e.g. a duplicate post) — don't leave the gate latched.
                update::set_prompt_active(false);
            }
            0
        }
        WM_TIMER => {
            if w_param == TIMER_STATUS {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
                if !ptr.is_null() {
                    // Update status text
                    let base_status = if recorder::is_recording() {
                        "Recording..."
                    } else if player::is_playing() {
                        if player::is_loop_mode() {
                            "Playing (loop)"
                        } else {
                            "Playing..."
                        }
                    } else if network::is_listening() {
                        "Idle (Recv)"
                    } else {
                        "Idle"
                    };
                    let pet = pet_cycle::is_active();
                    let hp = monitor::is_active(monitor::Bar::Hp);
                    let mp = monitor::is_active(monitor::Bar::Mp);
                    let sp = monitor::is_active(monitor::Bar::Sp);
                    let burst_on = burst::is_active();
                    let mut status = base_status.to_string();
                    if pet { status.push_str(" [Pet]"); }
                    if hp { status.push_str(" [HP]"); }
                    if mp { status.push_str(" [MP]"); }
                    if sp { status.push_str(" [SP]"); }
                    if burst_on { status.push_str(" [BURST]"); }
                    SetWindowTextW((*ptr).hwnd_status, wide(&status).as_ptr());

                    // Update button text
                    let rec_text = if recorder::is_recording() {
                        "Stop"
                    } else {
                        "Record"
                    };
                    SetWindowTextW((*ptr).hwnd_btn_record, wide(rec_text).as_ptr());

                    let play_text = if player::is_playing() {
                        "Stop"
                    } else {
                        "Play"
                    };
                    SetWindowTextW((*ptr).hwnd_btn_play, wide(play_text).as_ptr());

                    let burst_text = if burst_on { "BURST ON" } else { "Burst Q" };
                    SetWindowTextW((*ptr).hwnd_btn_burst, wide(burst_text).as_ptr());
                }
            }
            0
        }
        WM_CLOSE => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolbarControls;
            if !ptr.is_null() {
                let _ = Box::from_raw(ptr); // free the controls struct
            }
            KillTimer(hwnd, TIMER_STATUS);
            DestroyWindow(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
