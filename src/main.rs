#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod burst;
mod config;
mod gui;
mod hotkeys;
mod monitor;
mod network;
mod pet_cycle;
mod player;
mod proximity;
mod recorder;
mod sequence;
mod storage;
mod timing;
mod win32_helpers;

use winapi::shared::minwindef::TRUE;
use winapi::um::winuser::*;

// Custom message posted by the burst worker to the main thread when it stops
// on its own (focus lost, app exit). Lets the toolbar refresh its visual.
pub const WM_APP_BURST_STOPPED: u32 = WM_APP + 2;

fn main() {
    // Raise the OS timer resolution to ~1ms for the whole process so the
    // playback/monitor/burst pacing can sleep accurately instead of busy
    // spin-waiting. Critical in VMs: a spin loop pins the guest vCPU and
    // starves the game's render thread, lagging the game while a sequence
    // plays. Released with timeEndPeriod(1) at shutdown.
    unsafe {
        winapi::um::timeapi::timeBeginPeriod(1);
    }

    // Belt-and-suspenders DPI awareness. The manifest already declares
    // PerMonitorV2, but if it didn't embed (broken build env) the process
    // would fall back to DPI-unaware and HP-monitor pixel coords would
    // silently desync from the picker on >100% scaling. Resolved dynamically
    // because winapi 0.3 doesn't expose SetProcessDpiAwarenessContext.
    unsafe {
        let lib = winapi::um::libloaderapi::LoadLibraryW(
            win32_helpers::wide("user32.dll").as_ptr(),
        );
        if !lib.is_null() {
            let name = b"SetProcessDpiAwarenessContext\0";
            let proc_addr = winapi::um::libloaderapi::GetProcAddress(
                lib,
                name.as_ptr() as *const i8,
            );
            if !proc_addr.is_null() {
                type SetCtx = unsafe extern "system" fn(isize) -> i32;
                let f: SetCtx = std::mem::transmute(proc_addr);
                let _ = f(-4); // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
            }
        }
    }

    // Load config
    let cfg = config::load_config();

    // Set loop mode and shuffle mode from config
    player::set_loop_mode(cfg.loop_playback);
    player::set_shuffle_mode(cfg.shuffle_queue);

    // Create GUI toolbar window
    let hwnd = gui::create_toolbar_window(&cfg);
    if hwnd.is_null() {
        eprintln!("[Cadence] Failed to create window.");
        return;
    }

    // Install global low-level keyboard hook for hotkeys
    // (works even when fullscreen games have focus, unlike RegisterHotKey)
    if !hotkeys::install_hook(cfg.record_vk, cfg.stop_vk) {
        unsafe {
            let msg = win32_helpers::wide("Failed to install hotkey hook.");
            let title = win32_helpers::wide("Cadence Error");
            MessageBoxW(
                std::ptr::null_mut(),
                msg.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
        return;
    }

    // Load all sequence hotkey bindings at startup
    let bindings = gui::load_all_bindings();
    hotkeys::set_sequence_bindings(bindings);

    // Load remote hotkey bindings
    let remote_binds: Vec<(u32, u16, String)> = cfg.remote_bindings
        .iter()
        .map(|b| (b.modifiers, b.vk_code, b.sequence_name.clone()))
        .collect();
    hotkeys::set_remote_bindings(remote_binds);

    // Load queue hotkey binding
    if let Some(qvk) = cfg.queue_vk {
        hotkeys::set_queue_vk(Some(qvk));
    }

    // Load burst hotkey binding (None until configured)
    if cfg.burst_vk != 0 {
        hotkeys::set_burst_vk(Some(cfg.burst_vk));
    }

    // Wire the burst worker so it can wake the UI when it stops itself
    // (focus loss). Use the main thread ID we already store in hotkeys.
    let main_tid = unsafe { winapi::um::processthreadsapi::GetCurrentThreadId() };
    burst::set_notify(main_tid, WM_APP_BURST_STOPPED);

    // Let proximity notify the toolbar to uncheck "Det" when a detection fires (one-shot).
    proximity::set_notify(hwnd as isize, gui::WM_APP_PROXIMITY_HIT);

    // Auto-start receiver if configured
    if cfg.remote_auto_listen && cfg.remote_port > 0 {
        let password = if cfg.remote_password.is_empty() {
            None
        } else {
            Some(cfg.remote_password.clone())
        };
        if let Err(e) = network::start_listener(cfg.remote_port, password) {
            eprintln!("[Cadence] Auto-listen failed: {}", e);
        }
    }

    // Auto-start pet cycle if configured
    if cfg.pet_cycle_enabled {
        pet_cycle::start(cfg.pet_cycle_interval_secs);
    }

    // Auto-start bar monitors if configured. MP/SP share HP's window anchor.
    if cfg.hp_monitor_enabled && cfg.hp_monitor_color != 0 {
        monitor::set_bar(
            monitor::Bar::Hp,
            cfg.hp_monitor_window_class.clone(),
            cfg.hp_monitor_window_title.clone(),
            cfg.hp_monitor_x,
            cfg.hp_monitor_y,
            cfg.hp_monitor_color,
        );
    }
    if cfg.mp_monitor_enabled && cfg.mp_monitor_color != 0 {
        monitor::set_bar(
            monitor::Bar::Mp,
            cfg.hp_monitor_window_class.clone(),
            cfg.hp_monitor_window_title.clone(),
            cfg.mp_monitor_x,
            cfg.mp_monitor_y,
            cfg.mp_monitor_color,
        );
    }
    if cfg.sp_monitor_enabled && cfg.sp_monitor_color != 0 {
        monitor::set_bar(
            monitor::Bar::Sp,
            cfg.hp_monitor_window_class.clone(),
            cfg.hp_monitor_window_title.clone(),
            cfg.sp_monitor_x,
            cfg.sp_monitor_y,
            cfg.sp_monitor_color,
        );
    }

    // Load the proximity ignore list into the live detector unconditionally, so it mirrors
    // config even when detection starts disabled (keeps the Settings save from wiping it).
    proximity::set_ignored(cfg.proximity_ignore.clone());
    proximity::set_reaction(cfg.proximity_sequence.clone());

    // Auto-start proximity alert if configured
    if cfg.proximity_enabled {
        proximity::start(
            cfg.proximity_vk,
            cfg.proximity_iface.clone(),
            cfg.proximity_server_ip.clone(),
            cfg.proximity_cooldown_ms,
        );
    }

    // Load default sequence (or fall back to last saved) into LAST_EVENTS
    let load_name = cfg.default_sequence.clone()
        .or_else(|| storage::list_sequences().ok().and_then(|n| n.last().cloned()));
    if let Some(name) = load_name {
        if let Ok(seq) = storage::load_sequence(&name) {
            *win32_helpers::lock_or_recover(&gui::LAST_EVENTS) = Some(seq.events);
        }
    }

    // Win32 message loop
    let mut msg = unsafe { std::mem::zeroed::<MSG>() };
    loop {
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            break;
        }

        // Handle burst worker stop notifications (focus loss / shutdown).
        if msg.message == WM_APP_BURST_STOPPED {
            unsafe {
                let toplevel = FindWindowW(
                    win32_helpers::wide("CadenceMain").as_ptr(),
                    std::ptr::null(),
                );
                if !toplevel.is_null() {
                    InvalidateRect(toplevel, std::ptr::null(), TRUE);
                }
            }
            continue;
        }

        // Handle hotkey messages from our low-level hook
        if msg.message == hotkeys::WM_APP_HOTKEY {
            let hotkey_id = msg.wParam as i32;
            match hotkey_id {
                hotkeys::HOTKEY_TOGGLE_RECORD => gui::handle_record_toggle(),
                hotkeys::HOTKEY_PLAY_STOP => gui::handle_play_toggle(),
                hotkeys::HOTKEY_PLAY_QUEUE => gui::handle_play_queue_hotkey(),
                hotkeys::HOTKEY_BURST_TOGGLE => {
                    let cfg = config::cached_config();
                    burst::toggle(
                        cfg.burst_rate_hz,
                        cfg.hp_monitor_window_class.clone(),
                        cfg.hp_monitor_window_title.clone(),
                    );
                }
                hotkeys::HOTKEY_PLAY_SEQUENCE => {
                    let vk = msg.lParam as u16;
                    gui::handle_play_sequence(vk);
                }
                hotkeys::HOTKEY_REMOTE_SEND => {
                    let index = msg.lParam as usize;
                    if let Some(seq_name) = hotkeys::remote_binding_at(index) {
                        let cfg = config::cached_config();
                        if !cfg.remote_hosts.is_empty() {
                            let port = cfg.remote_port;
                            let pw = if cfg.remote_password.is_empty() {
                                None
                            } else {
                                Some(cfg.remote_password)
                            };
                            let cmd = format!("PLAY {}", seq_name);
                            for host in cfg.remote_hosts {
                                let cmd = cmd.clone();
                                let pw = pw.clone();
                                std::thread::spawn(move || {
                                    if let Err(e) = network::send_command(&host, port, pw.as_deref(), &cmd) {
                                        eprintln!("[Cadence] Remote send to {} failed: {}", host, e);
                                    }
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup
    proximity::stop();
    burst::stop();
    monitor::stop_all();
    pet_cycle::stop();
    network::stop_listener();
    hotkeys::uninstall_hook();

    unsafe {
        winapi::um::timeapi::timeEndPeriod(1);
    }
}
