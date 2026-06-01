use crate::win32_helpers::{wide, create_control, register_and_create_dialog, populate_key_combo, lock_or_recover, BURST_KEY_OPTIONS, KEY_OPTIONS};
use crate::{burst, config, hotkeys};
use super::*;
use super::toolbar::ToolbarControls;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

const GA_ROOT: u32 = 2;

static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);
// Per-bar sampled color, indexed 0=HP, 1=MP, 2=SP.
static SAMPLED_COLOR: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
// Window class/title are shared across bars (same game window).
static SAMPLED_CLASS: Mutex<String> = Mutex::new(String::new());
static SAMPLED_TITLE: Mutex<String> = Mutex::new(String::new());
static PICKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Which bar the live picker is currently capturing for (0=HP,1=MP,2=SP).
static PICK_TARGET: AtomicU8 = AtomicU8::new(0);

/// Map a bar target (0=HP,1=MP,2=SP) to its dialog control IDs:
/// (edit_x, edit_y, color_label, pick_button, live_label).
fn pick_ctrl_ids(target: u8) -> (u16, u16, u16, u16, u16) {
    match target {
        1 => (IDC_EDIT_MP_X, IDC_EDIT_MP_Y, IDC_STATIC_MP_COLOR, IDC_BTN_MP_PICK, IDC_STATIC_MP_LIVE),
        2 => (IDC_EDIT_SP_X, IDC_EDIT_SP_Y, IDC_STATIC_SP_COLOR, IDC_BTN_SP_PICK, IDC_STATIC_SP_LIVE),
        _ => (IDC_EDIT_HP_X, IDC_EDIT_HP_Y, IDC_STATIC_HP_COLOR, IDC_BTN_HP_PICK, IDC_STATIC_HP_LIVE),
    }
}

const BAR_LABELS: [&str; 3] = ["HP", "MP", "SP"];

/// Build one monitor section (header, X/Y edits, Pick/Sample buttons, color +
/// live labels) at vertical origin `y0`. Mirrors the hand-laid HP section.
#[allow(clippy::too_many_arguments)]
unsafe fn create_section_controls(
    hwnd: HWND,
    hinstance: HINSTANCE,
    font: HFONT,
    label: &str,
    y0: i32,
    edit_x: u16,
    edit_y: u16,
    pick: u16,
    sample: u16,
    color: u16,
    live: u16,
) {
    create_control(
        hwnd, hinstance, font, "STATIC", &format!("\u{2014} {} Monitor \u{2014}", label),
        WS_CHILD | WS_VISIBLE | SS_CENTER, 0, 12, y0, 272, 18, 0,
    );
    create_control(
        hwnd, hinstance, font, "STATIC", "X:",
        WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, y0 + 26, 16, 20, 0,
    );
    create_control(
        hwnd, hinstance, font, "EDIT", "",
        WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0, 30, y0 + 24, 70, 22, edit_x,
    );
    create_control(
        hwnd, hinstance, font, "STATIC", "Y:",
        WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 112, y0 + 26, 16, 20, 0,
    );
    create_control(
        hwnd, hinstance, font, "EDIT", "",
        WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0, 130, y0 + 24, 70, 22, edit_y,
    );
    create_control(
        hwnd, hinstance, font, "BUTTON", "Pick",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 210, y0 + 24, 60, 22, pick,
    );
    create_control(
        hwnd, hinstance, font, "BUTTON", "Sample",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 12, y0 + 56, 60, 24, sample,
    );
    create_control(
        hwnd, hinstance, font, "STATIC", "(not sampled)",
        WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 80, y0 + 59, 190, 20, color,
    );
    create_control(
        hwnd, hinstance, font, "STATIC", "",
        WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, y0 + 86, 268, 20, live,
    );
}

/// Read an integer from a numeric edit control, defaulting to 0.
unsafe fn read_edit_i32(hwnd: HWND, id: u16) -> i32 {
    let mut buf = [0u16; 16];
    GetWindowTextW(GetDlgItem(hwnd, id as i32), buf.as_mut_ptr(), 16);
    let s: String = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
    s.parse().unwrap_or(0)
}

/// Pre-fill a section's X/Y edits and color label + sampled-color slot from config.
unsafe fn prepopulate_section(hwnd: HWND, target: u8, x: i32, y: i32, color: u32) {
    let (edit_x, edit_y, color_id, _pick, _live) = pick_ctrl_ids(target);
    if x != 0 || y != 0 {
        let x_text = wide(&x.to_string());
        let y_text = wide(&y.to_string());
        SetWindowTextW(GetDlgItem(hwnd, edit_x as i32), x_text.as_ptr());
        SetWindowTextW(GetDlgItem(hwnd, edit_y as i32), y_text.as_ptr());
    }
    if color != 0 {
        SAMPLED_COLOR[target as usize].store(color, Ordering::Release);
        let r = color & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = (color >> 16) & 0xFF;
        let text = wide(&format!("R:{} G:{} B:{}", r, g, b));
        SetWindowTextW(GetDlgItem(hwnd, color_id as i32), text.as_ptr());
    } else {
        SAMPLED_COLOR[target as usize].store(0, Ordering::Release);
    }
}

pub unsafe fn show_settings_dialog(parent: HWND) {
    let existing = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    // Position near the parent toolbar
    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);

    let sx = parent_rect.left;
    let sy = parent_rect.bottom + 4;

    let hwnd = register_and_create_dialog(
        "CadenceSettings", "Settings",
        settings_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 300, 630,
        parent, hinstance,
    );
    SETTINGS_HWND.store(hwnd as isize, Ordering::Release);
}

unsafe extern "system" fn settings_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = GetStockObject(DEFAULT_GUI_FONT as i32) as HFONT;

            // "Record Key:" label
            create_control(
                hwnd, hinstance, font, "STATIC", "Record Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 16, 80, 20, 0,
            );

            // Record key combobox
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 12, 148, 200, IDC_COMBO_RECORD_KEY,
            );

            // "Stop Key:" label
            create_control(
                hwnd, hinstance, font, "STATIC", "Stop Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 52, 80, 20, 0,
            );

            // Stop key combobox
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 48, 148, 200, IDC_COMBO_PLAY_KEY,
            );

            // Populate comboboxes
            let (current_rec, current_stop) = hotkeys::current_hotkeys();
            let h_combo_rec = GetDlgItem(hwnd, IDC_COMBO_RECORD_KEY as i32);
            let h_combo_play = GetDlgItem(hwnd, IDC_COMBO_PLAY_KEY as i32);
            populate_key_combo(h_combo_rec, KEY_OPTIONS, Some(current_rec));
            populate_key_combo(h_combo_play, KEY_OPTIONS, Some(current_stop));

            // "Queue Key:" label
            create_control(
                hwnd, hinstance, font, "STATIC", "Queue Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 88, 80, 20, 0,
            );

            // Queue key combobox
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 84, 148, 200, IDC_COMBO_QUEUE_KEY,
            );

            // Populate queue key combo with "(None)" option
            let h_combo_queue = GetDlgItem(hwnd, IDC_COMBO_QUEUE_KEY as i32);
            let none_text = wide("(None)");
            SendMessageW(h_combo_queue, CB_ADDSTRING, 0, none_text.as_ptr() as LPARAM);

            let current_queue_vk = hotkeys::current_queue_vk();
            if current_queue_vk.is_none() {
                SendMessageW(h_combo_queue, CB_SETCURSEL, 0, 0);
            }
            for (i, (vk, name)) in KEY_OPTIONS.iter().enumerate() {
                let wname = wide(name);
                SendMessageW(h_combo_queue, CB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
                if current_queue_vk == Some(*vk) {
                    SendMessageW(h_combo_queue, CB_SETCURSEL, (i + 1) as WPARAM, 0);
                }
            }

            // -- HP Monitor section --
            create_control(
                hwnd, hinstance, font, "STATIC", "— HP Monitor —",
                WS_CHILD | WS_VISIBLE | SS_CENTER, 0,
                12, 124, 272, 18, 0,
            );

            // X coordinate
            create_control(
                hwnd, hinstance, font, "STATIC", "X:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 150, 16, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                30, 148, 70, 22, IDC_EDIT_HP_X,
            );

            // Y coordinate
            create_control(
                hwnd, hinstance, font, "STATIC", "Y:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                112, 150, 16, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                130, 148, 70, 22, IDC_EDIT_HP_Y,
            );

            // Pick button — click to enter screen pixel picker mode
            create_control(
                hwnd, hinstance, font, "BUTTON", "Pick",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                210, 148, 60, 22, IDC_BTN_HP_PICK,
            );

            // Sample button (manual)
            create_control(
                hwnd, hinstance, font, "BUTTON", "Sample",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                12, 180, 60, 24, IDC_BTN_HP_SAMPLE,
            );

            // Color preview label
            create_control(
                hwnd, hinstance, font, "STATIC", "(not sampled)",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                80, 183, 190, 20, IDC_STATIC_HP_COLOR,
            );

            // Live picker display — shows cursor X, Y and color while picking
            create_control(
                hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 210, 268, 20, IDC_STATIC_HP_LIVE,
            );

            // Pre-populate HP fields from config
            let parent = GetParent(hwnd);
            let parent_ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
            if !parent_ptr.is_null() {
                let cfg = &(*parent_ptr).config;
                if cfg.hp_monitor_x != 0 || cfg.hp_monitor_y != 0 {
                    let x_text = wide(&cfg.hp_monitor_x.to_string());
                    let y_text = wide(&cfg.hp_monitor_y.to_string());
                    SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_HP_X as i32), x_text.as_ptr());
                    SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_HP_Y as i32), y_text.as_ptr());
                }
                if cfg.hp_monitor_color != 0 {
                    SAMPLED_COLOR[0].store(cfg.hp_monitor_color, Ordering::Release);
                    let r = cfg.hp_monitor_color & 0xFF;
                    let g = (cfg.hp_monitor_color >> 8) & 0xFF;
                    let b = (cfg.hp_monitor_color >> 16) & 0xFF;
                    let color_text = wide(&format!("R:{} G:{} B:{}", r, g, b));
                    SetWindowTextW(GetDlgItem(hwnd, IDC_STATIC_HP_COLOR as i32), color_text.as_ptr());
                } else {
                    SAMPLED_COLOR[0].store(0, Ordering::Release);
                }
                *lock_or_recover(&SAMPLED_CLASS) = cfg.hp_monitor_window_class.clone();
                *lock_or_recover(&SAMPLED_TITLE) = cfg.hp_monitor_window_title.clone();
            }

            // -- MP Monitor section --
            create_section_controls(hwnd, hinstance, font, "MP", 340,
                IDC_EDIT_MP_X, IDC_EDIT_MP_Y, IDC_BTN_MP_PICK, IDC_BTN_MP_SAMPLE,
                IDC_STATIC_MP_COLOR, IDC_STATIC_MP_LIVE);
            // -- SP Monitor section --
            create_section_controls(hwnd, hinstance, font, "SP", 452,
                IDC_EDIT_SP_X, IDC_EDIT_SP_Y, IDC_BTN_SP_PICK, IDC_BTN_SP_SAMPLE,
                IDC_STATIC_SP_COLOR, IDC_STATIC_SP_LIVE);

            // Pre-populate MP/SP fields from config
            if !parent_ptr.is_null() {
                let cfg = &(*parent_ptr).config;
                prepopulate_section(hwnd, 1, cfg.mp_monitor_x, cfg.mp_monitor_y, cfg.mp_monitor_color);
                prepopulate_section(hwnd, 2, cfg.sp_monitor_x, cfg.sp_monitor_y, cfg.sp_monitor_color);
            }

            // -- Burst Q section --
            create_control(
                hwnd, hinstance, font, "STATIC", "— Burst Q —",
                WS_CHILD | WS_VISIBLE | SS_CENTER, 0,
                12, 244, 272, 18, 0,
            );

            // Hotkey label + combobox
            create_control(
                hwnd, hinstance, font, "STATIC", "Hotkey:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 272, 50, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                66, 268, 130, 200, IDC_COMBO_BURST_KEY,
            );

            let h_combo_burst = GetDlgItem(hwnd, IDC_COMBO_BURST_KEY as i32);
            let none_text = wide("(None)");
            SendMessageW(h_combo_burst, CB_ADDSTRING, 0, none_text.as_ptr() as LPARAM);
            let current_burst_vk = hotkeys::current_burst_vk();
            if current_burst_vk.is_none() {
                SendMessageW(h_combo_burst, CB_SETCURSEL, 0, 0);
            }
            for (i, (vk, name)) in BURST_KEY_OPTIONS.iter().enumerate() {
                let wname = wide(name);
                SendMessageW(h_combo_burst, CB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
                if current_burst_vk == Some(*vk) {
                    SendMessageW(h_combo_burst, CB_SETCURSEL, (i + 1) as WPARAM, 0);
                }
            }

            // Rate label + edit (Hz)
            create_control(
                hwnd, hinstance, font, "STATIC", "Rate (Hz):",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 304, 60, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                76, 300, 60, 22, IDC_EDIT_BURST_RATE,
            );
            create_control(
                hwnd, hinstance, font, "STATIC", "(50-200, default 100)",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                144, 304, 140, 20, 0,
            );

            // Pre-populate burst rate from config
            let parent2 = GetParent(hwnd);
            let parent2_ptr = GetWindowLongPtrW(parent2, GWLP_USERDATA) as *mut ToolbarControls;
            if !parent2_ptr.is_null() {
                let cfg = &(*parent2_ptr).config;
                let rate_text = wide(&cfg.burst_rate_hz.to_string());
                SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_BURST_RATE as i32), rate_text.as_ptr());
            }

            let _ = parent2_ptr;

            // OK button (below the SP monitor section)
            create_control(
                hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                115, 572, 70, 28, IDC_BTN_SETTINGS_OK,
            );

            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            if control_id == IDC_BTN_HP_PICK
                || control_id == IDC_BTN_MP_PICK
                || control_id == IDC_BTN_SP_PICK
            {
                let target: u8 = if control_id == IDC_BTN_MP_PICK {
                    1
                } else if control_id == IDC_BTN_SP_PICK {
                    2
                } else {
                    0
                };
                // Toggle pick mode (only one bar picks at a time)
                if PICKING.load(Ordering::Acquire) {
                    // Stop picking — reset whichever section was active
                    PICKING.store(false, Ordering::Release);
                    KillTimer(hwnd, TIMER_HP_PICK);
                    let active = PICK_TARGET.load(Ordering::Acquire);
                    let (_x, _y, _c, active_pick, active_live) = pick_ctrl_ids(active);
                    SetWindowTextW(GetDlgItem(hwnd, active_pick as i32), wide("Pick").as_ptr());
                    SetWindowTextW(GetDlgItem(hwnd, active_live as i32), wide("").as_ptr());
                } else {
                    // Start picking — poll cursor every 50ms
                    PICK_TARGET.store(target, Ordering::Release);
                    PICKING.store(true, Ordering::Release);
                    SetTimer(hwnd, TIMER_HP_PICK, 50, None);
                    let (_x, _y, _c, pick_id, live_id) = pick_ctrl_ids(target);
                    SetWindowTextW(GetDlgItem(hwnd, pick_id as i32), wide("Stop").as_ptr());
                    let msg = format!("Move to {} pixel, press INSERT", BAR_LABELS[target as usize]);
                    SetWindowTextW(GetDlgItem(hwnd, live_id as i32), wide(&msg).as_ptr());
                }
                return 0;
            } else if control_id == IDC_BTN_HP_SAMPLE
                || control_id == IDC_BTN_MP_SAMPLE
                || control_id == IDC_BTN_SP_SAMPLE
            {
                let target: u8 = if control_id == IDC_BTN_MP_SAMPLE {
                    1
                } else if control_id == IDC_BTN_SP_SAMPLE {
                    2
                } else {
                    0
                };
                let (edit_x_id, edit_y_id, color_id, _pick, _live) = pick_ctrl_ids(target);

                // Read X, Y from edit boxes and sample pixel color
                let mut buf_x = [0u16; 16];
                let mut buf_y = [0u16; 16];
                GetWindowTextW(GetDlgItem(hwnd, edit_x_id as i32), buf_x.as_mut_ptr(), 16);
                GetWindowTextW(GetDlgItem(hwnd, edit_y_id as i32), buf_y.as_mut_ptr(), 16);

                let x_str: String = buf_x.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
                let y_str: String = buf_y.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();

                if let (Ok(x), Ok(y)) = (x_str.parse::<i32>(), y_str.parse::<i32>()) {
                    let class = lock_or_recover(&SAMPLED_CLASS).clone();
                    let title = lock_or_recover(&SAMPLED_TITLE).clone();

                    let (sample_hwnd, sample_x, sample_y) = if class.is_empty() {
                        (std::ptr::null_mut(), x, y)
                    } else {
                        let class_w = wide(&class);
                        let mut found: HWND = FindWindowExW(
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            class_w.as_ptr(),
                            std::ptr::null(),
                        );
                        if !title.is_empty() {
                            while !found.is_null() {
                                let mut buf = [0u16; 256];
                                let len = GetWindowTextW(found, buf.as_mut_ptr(), buf.len() as i32) as usize;
                                let actual = String::from_utf16_lossy(&buf[..len]);
                                if actual.starts_with(&title) {
                                    break;
                                }
                                found = FindWindowExW(
                                    std::ptr::null_mut(),
                                    found,
                                    class_w.as_ptr(),
                                    std::ptr::null(),
                                );
                            }
                            if found.is_null() {
                                found = FindWindowW(class_w.as_ptr(), std::ptr::null());
                            }
                        }
                        (found, x, y)
                    };

                    if !class.is_empty() && sample_hwnd.is_null() {
                        let text = wide("(target window not found)");
                        SetWindowTextW(GetDlgItem(hwnd, color_id as i32), text.as_ptr());
                    } else {
                        let hdc = GetDC(sample_hwnd);
                        if !hdc.is_null() {
                            let color = GetPixel(hdc, sample_x, sample_y);
                            ReleaseDC(sample_hwnd, hdc);

                            if color != 0xFFFFFFFF {
                                SAMPLED_COLOR[target as usize].store(color, Ordering::Release);
                                let r = color & 0xFF;
                                let g = (color >> 8) & 0xFF;
                                let b = (color >> 16) & 0xFF;
                                let text = wide(&format!("R:{} G:{} B:{}", r, g, b));
                                SetWindowTextW(GetDlgItem(hwnd, color_id as i32), text.as_ptr());
                            } else {
                                let text = wide("(invalid pixel)");
                                SetWindowTextW(GetDlgItem(hwnd, color_id as i32), text.as_ptr());
                            }
                        }
                    }
                } else {
                    let msg = wide("Enter valid X and Y coordinates.");
                    let title = wide(&format!("{} Monitor", BAR_LABELS[target as usize]));
                    MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING);
                }
                return 0;
            } else if control_id == IDC_BTN_SETTINGS_OK {
                // Read selections
                let h_combo_rec = GetDlgItem(hwnd, IDC_COMBO_RECORD_KEY as i32);
                let h_combo_play = GetDlgItem(hwnd, IDC_COMBO_PLAY_KEY as i32);

                let rec_idx = SendMessageW(h_combo_rec, CB_GETCURSEL, 0, 0) as usize;
                let play_idx = SendMessageW(h_combo_play, CB_GETCURSEL, 0, 0) as usize;

                if rec_idx < KEY_OPTIONS.len() && play_idx < KEY_OPTIONS.len() {
                    let new_rec_vk = KEY_OPTIONS[rec_idx].0;
                    let new_stop_vk = KEY_OPTIONS[play_idx].0;

                    // Read queue key selection
                    let h_combo_queue = GetDlgItem(hwnd, IDC_COMBO_QUEUE_KEY as i32);
                    let queue_idx = SendMessageW(h_combo_queue, CB_GETCURSEL, 0, 0) as usize;
                    let new_queue_vk: Option<u16> = if queue_idx == 0 {
                        None // "(None)" selected
                    } else if queue_idx - 1 < KEY_OPTIONS.len() {
                        Some(KEY_OPTIONS[queue_idx - 1].0)
                    } else {
                        None
                    };

                    if new_rec_vk == new_stop_vk {
                        let msg = wide("Record and Stop keys must be different!");
                        let title = wide("Error");
                        MessageBoxW(
                            hwnd,
                            msg.as_ptr(),
                            title.as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                        return 0;
                    }

                    if let Some(qvk) = new_queue_vk {
                        if qvk == new_rec_vk || qvk == new_stop_vk {
                            let msg = wide("Queue key must be different from Record and Stop keys!");
                            let title = wide("Error");
                            MessageBoxW(
                                hwnd,
                                msg.as_ptr(),
                                title.as_ptr(),
                                MB_OK | MB_ICONERROR,
                            );
                            return 0;
                        }
                    }

                    // Read burst hotkey selection
                    let h_combo_burst = GetDlgItem(hwnd, IDC_COMBO_BURST_KEY as i32);
                    let burst_idx = SendMessageW(h_combo_burst, CB_GETCURSEL, 0, 0) as usize;
                    let new_burst_vk: Option<u16> = if burst_idx == 0 {
                        None
                    } else if burst_idx - 1 < BURST_KEY_OPTIONS.len() {
                        Some(BURST_KEY_OPTIONS[burst_idx - 1].0)
                    } else {
                        None
                    };

                    if let Some(bvk) = new_burst_vk {
                        if bvk == new_rec_vk || bvk == new_stop_vk
                            || new_queue_vk == Some(bvk)
                        {
                            let msg = wide("Burst hotkey must be different from Record, Stop, and Queue keys!");
                            let title = wide("Error");
                            MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
                            return 0;
                        }
                    }

                    // Read burst rate
                    let mut buf_rate = [0u16; 16];
                    GetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_BURST_RATE as i32), buf_rate.as_mut_ptr(), 16);
                    let rate_str: String = buf_rate.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
                    let new_burst_rate: u32 = rate_str.parse().unwrap_or(100);
                    let new_burst_rate = new_burst_rate.clamp(50, 200);

                    if hotkeys::reregister_hotkeys(new_rec_vk, new_stop_vk) {
                        hotkeys::set_queue_vk(new_queue_vk);
                        hotkeys::set_burst_vk(new_burst_vk);
                        let parent = GetParent(hwnd);
                        let ptr =
                            GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ToolbarControls;
                        if !ptr.is_null() {
                            (*ptr).config.record_vk = new_rec_vk;
                            (*ptr).config.stop_vk = new_stop_vk;
                            (*ptr).config.queue_vk = new_queue_vk;
                            (*ptr).config.burst_vk = new_burst_vk.unwrap_or(0);
                            (*ptr).config.burst_rate_hz = new_burst_rate;

                            // Save HP monitor settings. MP/SP share the window anchor.
                            (*ptr).config.hp_monitor_x = read_edit_i32(hwnd, IDC_EDIT_HP_X);
                            (*ptr).config.hp_monitor_y = read_edit_i32(hwnd, IDC_EDIT_HP_Y);
                            (*ptr).config.hp_monitor_color = SAMPLED_COLOR[0].load(Ordering::Acquire);
                            (*ptr).config.hp_monitor_window_class =
                                lock_or_recover(&SAMPLED_CLASS).clone();
                            (*ptr).config.hp_monitor_window_title =
                                lock_or_recover(&SAMPLED_TITLE).clone();

                            // Save MP monitor settings
                            (*ptr).config.mp_monitor_x = read_edit_i32(hwnd, IDC_EDIT_MP_X);
                            (*ptr).config.mp_monitor_y = read_edit_i32(hwnd, IDC_EDIT_MP_Y);
                            (*ptr).config.mp_monitor_color = SAMPLED_COLOR[1].load(Ordering::Acquire);

                            // Save SP monitor settings
                            (*ptr).config.sp_monitor_x = read_edit_i32(hwnd, IDC_EDIT_SP_X);
                            (*ptr).config.sp_monitor_y = read_edit_i32(hwnd, IDC_EDIT_SP_Y);
                            (*ptr).config.sp_monitor_color = SAMPLED_COLOR[2].load(Ordering::Acquire);

                            if let Err(e) = config::save_config(&(*ptr).config) {
                                eprintln!("[Cadence] Config save failed: {}", e);
                            }

                            // Push the just-saved settings into the live monitor so a
                            // re-pick / color change takes effect immediately — no
                            // checkbox toggle or app restart needed. MP/SP share HP's
                            // window anchor (same game window).
                            let cfg = &(*ptr).config;
                            let wc = cfg.hp_monitor_window_class.clone();
                            let wt = cfg.hp_monitor_window_title.clone();
                            use crate::monitor::{self, Bar};
                            if cfg.hp_monitor_enabled && cfg.hp_monitor_color != 0 {
                                monitor::set_bar(Bar::Hp, wc.clone(), wt.clone(),
                                    cfg.hp_monitor_x, cfg.hp_monitor_y, cfg.hp_monitor_color);
                            } else {
                                monitor::disable_bar(Bar::Hp);
                            }
                            if cfg.mp_monitor_enabled && cfg.mp_monitor_color != 0 {
                                monitor::set_bar(Bar::Mp, wc.clone(), wt.clone(),
                                    cfg.mp_monitor_x, cfg.mp_monitor_y, cfg.mp_monitor_color);
                            } else {
                                monitor::disable_bar(Bar::Mp);
                            }
                            if cfg.sp_monitor_enabled && cfg.sp_monitor_color != 0 {
                                monitor::set_bar(Bar::Sp, wc, wt,
                                    cfg.sp_monitor_x, cfg.sp_monitor_y, cfg.sp_monitor_color);
                            } else {
                                monitor::disable_bar(Bar::Sp);
                            }
                        }
                        // If burst was running and the user changed the rate,
                        // the change takes effect on next toggle. Mention this
                        // visually only if needed — for now, keep existing burst.
                        let _ = burst::is_active();
                        DestroyWindow(hwnd);
                        SETTINGS_HWND.store(0, Ordering::Release);
                    } else {
                        let msg = wide("Failed to register hotkeys.\nKey may be in use.");
                        let title = wide("Error");
                        MessageBoxW(
                            hwnd,
                            msg.as_ptr(),
                            title.as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
            }
            0
        }
        WM_TIMER => {
            if w_param == TIMER_HP_PICK && PICKING.load(Ordering::Acquire) {
                let target = PICK_TARGET.load(Ordering::Acquire);
                let (edit_x_id, edit_y_id, color_id, pick_id, live_id) = pick_ctrl_ids(target);
                let mut pt: POINT = std::mem::zeroed();
                GetCursorPos(&mut pt);

                // Resolve the top-level window under the cursor. We anchor the
                // pixel coords to its client area so that moving the game window
                // (or relaunching) doesn't invalidate the pick.
                let raw_under = WindowFromPoint(pt);
                let top_under = if !raw_under.is_null() {
                    let t = GetAncestor(raw_under, GA_ROOT);
                    if t.is_null() { raw_under } else { t }
                } else {
                    std::ptr::null_mut()
                };

                let (under_class, under_title) = if !top_under.is_null() {
                    let mut cbuf = [0u16; 256];
                    let clen = GetClassNameW(top_under, cbuf.as_mut_ptr(), cbuf.len() as i32) as usize;
                    let cls = String::from_utf16_lossy(&cbuf[..clen]);
                    let mut tbuf = [0u16; 256];
                    let tlen = GetWindowTextW(top_under, tbuf.as_mut_ptr(), tbuf.len() as i32) as usize;
                    let ttl = String::from_utf16_lossy(&tbuf[..tlen]);
                    (cls, ttl)
                } else {
                    (String::new(), String::new())
                };

                // Pixel sampling stays on the desktop DC during preview so the
                // live readout matches what the user sees regardless of which
                // window is under the cursor.
                let hdc = GetDC(std::ptr::null_mut());
                let color = if !hdc.is_null() {
                    let c = GetPixel(hdc, pt.x, pt.y);
                    ReleaseDC(std::ptr::null_mut(), hdc);
                    c
                } else {
                    0xFFFFFFFF
                };

                let win_label = if under_class.is_empty() {
                    "(no window)".to_string()
                } else if under_title.is_empty() {
                    under_class.clone()
                } else {
                    format!("{} [{}]", under_title, under_class)
                };

                if color != 0xFFFFFFFF {
                    let r = color & 0xFF;
                    let g = (color >> 8) & 0xFF;
                    let b = (color >> 16) & 0xFF;
                    let text = wide(&format!(
                        "X:{} Y:{} R:{} G:{} B:{} | {}",
                        pt.x, pt.y, r, g, b, win_label
                    ));
                    SetWindowTextW(GetDlgItem(hwnd, live_id as i32), text.as_ptr());
                } else {
                    let text = wide(&format!(
                        "X:{} Y:{} (unreadable) | {}",
                        pt.x, pt.y, win_label
                    ));
                    SetWindowTextW(GetDlgItem(hwnd, live_id as i32), text.as_ptr());
                }

                let insert_down = GetAsyncKeyState(VK_INSERT) & (0x8000u16 as i16) != 0;
                if insert_down {
                    // Convert the captured screen point into the target window's
                    // client coords. If we couldn't resolve a top-level window,
                    // fall back to absolute coords (legacy behavior).
                    let mut client_pt = pt;
                    let anchored = !top_under.is_null()
                        && !under_class.is_empty()
                        && !under_class.starts_with("Cadence")
                        && ScreenToClient(top_under, &mut client_pt) != 0;

                    let (saved_x, saved_y, saved_class, saved_title) = if anchored {
                        (client_pt.x, client_pt.y, under_class.clone(), under_title.clone())
                    } else {
                        (pt.x, pt.y, String::new(), String::new())
                    };

                    let x_text = wide(&saved_x.to_string());
                    let y_text = wide(&saved_y.to_string());
                    SetWindowTextW(GetDlgItem(hwnd, edit_x_id as i32), x_text.as_ptr());
                    SetWindowTextW(GetDlgItem(hwnd, edit_y_id as i32), y_text.as_ptr());

                    if color != 0xFFFFFFFF {
                        SAMPLED_COLOR[target as usize].store(color, Ordering::Release);
                        let r = color & 0xFF;
                        let g = (color >> 8) & 0xFF;
                        let b = (color >> 16) & 0xFF;
                        let text = wide(&format!("R:{} G:{} B:{}", r, g, b));
                        SetWindowTextW(
                            GetDlgItem(hwnd, color_id as i32),
                            text.as_ptr(),
                        );
                    }

                    *lock_or_recover(&SAMPLED_CLASS) = saved_class;
                    *lock_or_recover(&SAMPLED_TITLE) = saved_title;

                    PICKING.store(false, Ordering::Release);
                    KillTimer(hwnd, TIMER_HP_PICK);
                    let text = wide("Pick");
                    SetWindowTextW(GetDlgItem(hwnd, pick_id as i32), text.as_ptr());
                    let captured_msg = if anchored {
                        format!("Captured: {} ({}, {})", win_label, saved_x, saved_y)
                    } else {
                        format!("Captured (legacy absolute): ({}, {})", saved_x, saved_y)
                    };
                    let text = wide(&captured_msg);
                    SetWindowTextW(GetDlgItem(hwnd, live_id as i32), text.as_ptr());
                }
            }
            0
        }
        WM_CLOSE => {
            if PICKING.load(Ordering::Acquire) {
                PICKING.store(false, Ordering::Release);
                KillTimer(hwnd, TIMER_HP_PICK);
            }
            DestroyWindow(hwnd);
            SETTINGS_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}
