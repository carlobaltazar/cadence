use crate::win32_helpers::{wide, create_control, register_and_create_dialog, populate_key_combo, lock_or_recover, dpi_for_window, scale, scaled_font, BURST_KEY_OPTIONS, KEY_OPTIONS, REMOTE_KEY_OPTIONS};
use crate::{burst, config, hotkeys, proximity};
use super::*;
use super::toolbar::ToolbarControls;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

const GA_ROOT: u32 = 2;

// Dialog design size (96-DPI units). Two columns — left: hotkeys + HP/MP/SP monitors,
// right: Burst Q + Proximity — so the whole dialog (OK button included) fits a
// 1024×768 screen even at 125% display scaling.
const DIALOG_W: i32 = 640;
const DIALOG_H: i32 = 560;

static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);
// Per-bar sampled color, indexed 0=HP, 1=MP, 2=SP.
static SAMPLED_COLOR: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
// Window class/title are shared across bars (same game window).
static SAMPLED_CLASS: Mutex<String> = Mutex::new(String::new());
static SAMPLED_TITLE: Mutex<String> = Mutex::new(String::new());
static PICKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Which bar the live picker is currently capturing for (0=HP,1=MP,2=SP).
static PICK_TARGET: AtomicU8 = AtomicU8::new(0);
// Capture device names, index-aligned to the proximity interface combo (after "(Auto-select)").
static PROX_DEVICES: Mutex<Vec<String>> = Mutex::new(Vec::new());

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

/// Stretch one width-following control to the right margin, preserving its top/left/height.
/// Comboboxes need an explicit dropdown height, so pass `combo = true` for them.
unsafe fn stretch_to_right(hwnd: HWND, id: u16, right: i32, dpi: u32, combo: bool) {
    let h = GetDlgItem(hwnd, id as i32);
    if h.is_null() {
        return;
    }
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(h, &mut r);
    let mut tl = POINT { x: r.left, y: r.top };
    ScreenToClient(hwnd, &mut tl);
    let new_w = (right - tl.x).max(scale(60, dpi));
    let new_h = if combo { scale(200, dpi) } else { r.bottom - r.top };
    MoveWindow(h, tl.x, tl.y, new_w, new_h, TRUE);
}

/// Reflow the width-following controls (key/interface/action combos, live-picker readouts,
/// section dividers) to the current client width and re-pin OK to the bottom. Runs after
/// creation and on every WM_SIZE.
unsafe fn layout(hwnd: HWND) {
    let dpi = dpi_for_window(hwnd);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (cw, ch) = (rc.right, rc.bottom);
    if cw <= 0 || ch <= 0 {
        return;
    }
    let m = scale(12, dpi);
    let right = cw - m;

    // Only the RIGHT column follows the window's right edge; the left column (hotkeys +
    // bar monitors) and the vertical divider keep their created widths.
    for id in [IDC_SETTINGS_DIV3, IDC_EDIT_PROX_WATCH] {
        stretch_to_right(hwnd, id, right, dpi, false);
    }
    // Right-column combos that span to the right edge.
    for id in [IDC_COMBO_PROX_IFACE, IDC_COMBO_PROX_ACTION, IDC_COMBO_PROX_TRIGGER] {
        stretch_to_right(hwnd, id, right, dpi, true);
    }

    // OK floats to the bottom-centre, but never above the bottom of the TALLEST column
    // (left: SP live readout; right: Players button) — so a short window can't make it
    // overlap content. The min-track size guarantees the client can always hold the floor.
    let ok_w = scale(70, dpi);
    let ok_h = scale(28, dpi);
    let ok = GetDlgItem(hwnd, IDC_BTN_SETTINGS_OK as i32);
    if !ok.is_null() {
        let gap = scale(12, dpi);
        let mut floor = m; // fallback if neither column-bottom control can be located
        for id in [IDC_STATIC_SP_LIVE, IDC_BTN_PROX_PLAYERS] {
            let h = GetDlgItem(hwnd, id as i32);
            if !h.is_null() {
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut r);
                let mut bl = POINT { x: r.left, y: r.bottom };
                ScreenToClient(hwnd, &mut bl);
                floor = floor.max(bl.y + gap);
            }
        }
        let ok_y = (ch - m - ok_h).max(floor).max(0);
        MoveWindow(ok, (cw - ok_w) / 2, ok_y, ok_w, ok_h, TRUE);

        // Stretch the vertical column divider down to the content floor.
        let div = GetDlgItem(hwnd, IDC_SETTINGS_DIV2 as i32);
        if !div.is_null() {
            let mut dr: RECT = std::mem::zeroed();
            GetWindowRect(div, &mut dr);
            let mut dtl = POINT { x: dr.left, y: dr.top };
            ScreenToClient(hwnd, &mut dtl);
            MoveWindow(div, dtl.x, dtl.y, dr.right - dr.left, (floor - gap - dtl.y).max(0), TRUE);
        }
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

    // Clamp the opening position to the work area so the dialog (and its OK button) can
    // never spawn below the bottom of a low-resolution screen. SPI_GETWORKAREA is the
    // primary monitor's work area — good enough, the toolbar lives there on the VMs.
    let dpi = dpi_for_window(parent);
    let (w_px, h_px) = (scale(DIALOG_W, dpi), scale(DIALOG_H, dpi));
    let mut work: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as *mut _, 0);
    let sx = parent_rect.left.min(work.right - w_px).max(work.left);
    let sy = (parent_rect.bottom + 4).min(work.bottom - h_px).max(work.top);

    // Version in the title so users can say which build they're on when reporting something.
    let title = format!("Settings \u{2014} Cadence {}", crate::update::current_version());
    let hwnd = register_and_create_dialog(
        "CadenceSettings", &title,
        settings_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        // Resizable: WS_THICKFRAME + WS_MAXIMIZEBOX let the user widen/maximize the dialog;
        // WM_SIZE reflows the width-following controls and re-pins OK to the bottom.
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MAXIMIZEBOX | WS_VISIBLE,
        sx, sy, DIALOG_W, DIALOG_H,
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
            let font = scaled_font(dpi_for_window(hwnd));

            // Sections are built top-to-bottom in visual order. Layout literals are authored at
            // 96 DPI and scaled by create_control; the width-following controls and the OK button
            // are re-placed by layout() (on create and on every resize).

            // -- Hotkeys --
            create_control(hwnd, hinstance, font, "STATIC", "Record Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, 16, 80, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 12, 180, 200, IDC_COMBO_RECORD_KEY);
            create_control(hwnd, hinstance, font, "STATIC", "Stop Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, 52, 80, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 48, 180, 200, IDC_COMBO_PLAY_KEY);
            create_control(hwnd, hinstance, font, "STATIC", "Queue Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, 88, 80, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                96, 84, 180, 200, IDC_COMBO_QUEUE_KEY);

            let (current_rec, current_stop) = hotkeys::current_hotkeys();
            populate_key_combo(GetDlgItem(hwnd, IDC_COMBO_RECORD_KEY as i32), KEY_OPTIONS, Some(current_rec));
            populate_key_combo(GetDlgItem(hwnd, IDC_COMBO_PLAY_KEY as i32), KEY_OPTIONS, Some(current_stop));

            // Queue combo leads with "(None)".
            let h_combo_queue = GetDlgItem(hwnd, IDC_COMBO_QUEUE_KEY as i32);
            SendMessageW(h_combo_queue, CB_ADDSTRING, 0, wide("(None)").as_ptr() as LPARAM);
            let current_queue_vk = hotkeys::current_queue_vk();
            if current_queue_vk.is_none() {
                SendMessageW(h_combo_queue, CB_SETCURSEL, 0, 0);
            }
            for (i, (vk, name)) in KEY_OPTIONS.iter().enumerate() {
                SendMessageW(h_combo_queue, CB_ADDSTRING, 0, wide(name).as_ptr() as LPARAM);
                if current_queue_vk == Some(*vk) {
                    SendMessageW(h_combo_queue, CB_SETCURSEL, (i + 1) as WPARAM, 0);
                }
            }

            create_control(hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_ETCHEDHORZ, 0, 12, 116, 272, 2, IDC_SETTINGS_DIV1);

            // -- Bar monitors (HP / MP / SP), all via the shared helper so they're identical --
            create_section_controls(hwnd, hinstance, font, "HP", 128,
                IDC_EDIT_HP_X, IDC_EDIT_HP_Y, IDC_BTN_HP_PICK, IDC_BTN_HP_SAMPLE,
                IDC_STATIC_HP_COLOR, IDC_STATIC_HP_LIVE);
            create_section_controls(hwnd, hinstance, font, "MP", 240,
                IDC_EDIT_MP_X, IDC_EDIT_MP_Y, IDC_BTN_MP_PICK, IDC_BTN_MP_SAMPLE,
                IDC_STATIC_MP_COLOR, IDC_STATIC_MP_LIVE);
            create_section_controls(hwnd, hinstance, font, "SP", 352,
                IDC_EDIT_SP_X, IDC_EDIT_SP_Y, IDC_BTN_SP_PICK, IDC_BTN_SP_SAMPLE,
                IDC_STATIC_SP_COLOR, IDC_STATIC_SP_LIVE);

            // Vertical divider between the two columns (left: hotkeys + bar monitors,
            // right: Burst + Proximity). Repurposes the old DIV2 id.
            create_control(hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_ETCHEDVERT, 0, 307, 12, 2, 446, IDC_SETTINGS_DIV2);

            // -- Burst Q (right column) --
            create_control(hwnd, hinstance, font, "STATIC", "\u{2014} Burst Q \u{2014}",
                WS_CHILD | WS_VISIBLE | SS_CENTER, 0, 332, 12, 272, 18, 0);
            create_control(hwnd, hinstance, font, "STATIC", "Hotkey:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 42, 50, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                386, 38, 120, 200, IDC_COMBO_BURST_KEY);
            let h_combo_burst = GetDlgItem(hwnd, IDC_COMBO_BURST_KEY as i32);
            SendMessageW(h_combo_burst, CB_ADDSTRING, 0, wide("(None)").as_ptr() as LPARAM);
            let current_burst_vk = hotkeys::current_burst_vk();
            if current_burst_vk.is_none() {
                SendMessageW(h_combo_burst, CB_SETCURSEL, 0, 0);
            }
            for (i, (vk, name)) in BURST_KEY_OPTIONS.iter().enumerate() {
                SendMessageW(h_combo_burst, CB_ADDSTRING, 0, wide(name).as_ptr() as LPARAM);
                if current_burst_vk == Some(*vk) {
                    SendMessageW(h_combo_burst, CB_SETCURSEL, (i + 1) as WPARAM, 0);
                }
            }
            create_control(hwnd, hinstance, font, "STATIC", "Rate (Hz):",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 72, 60, 20, 0);
            create_control(hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32, 0,
                396, 68, 60, 22, IDC_EDIT_BURST_RATE);
            create_control(hwnd, hinstance, font, "STATIC", "(50-200, default 100)",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 464, 72, 140, 20, 0);

            create_control(hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_ETCHEDHORZ, 0, 332, 104, 272, 2, IDC_SETTINGS_DIV3);

            // -- Proximity Alert --
            // Configuration only. Arming/disarming detection is the toolbar "Det" checkbox
            // (one control, one source of truth). Server IP is always auto-discovered.
            let pcfg = config::cached_config();
            create_control(hwnd, hinstance, font, "STATIC", "\u{2014} Proximity Alert \u{2014}",
                WS_CHILD | WS_VISIBLE | SS_CENTER, 0, 332, 116, 272, 18, 0);
            create_control(hwnd, hinstance, font, "STATIC", "Interface:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 150, 58, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                392, 146, 212, 240, IDC_COMBO_PROX_IFACE);
            let h_iface = GetDlgItem(hwnd, IDC_COMBO_PROX_IFACE as i32);
            SendMessageW(h_iface, CB_ADDSTRING, 0, wide("(Auto-select)").as_ptr() as LPARAM);
            let devices = proximity::list_devices();
            let mut dev_names: Vec<String> = Vec::new();
            let mut sel_iface: usize = 0;
            for (i, (name, desc)) in devices.iter().enumerate() {
                let label = if desc.is_empty() { name.clone() } else { desc.clone() };
                SendMessageW(h_iface, CB_ADDSTRING, 0, wide(&label).as_ptr() as LPARAM);
                if *name == pcfg.proximity_iface {
                    sel_iface = i + 1;
                }
                dev_names.push(name.clone());
            }
            SendMessageW(h_iface, CB_SETCURSEL, sel_iface as WPARAM, 0);
            *lock_or_recover(&PROX_DEVICES) = dev_names;
            create_control(hwnd, hinstance, font, "STATIC", "Key:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 184, 28, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                362, 180, 84, 200, IDC_COMBO_PROX_KEY);
            populate_key_combo(GetDlgItem(hwnd, IDC_COMBO_PROX_KEY as i32), REMOTE_KEY_OPTIONS, Some(pcfg.proximity_vk));
            // Trigger: react to everyone, or only to staff (see the GM names box below).
            create_control(hwnd, hinstance, font, "STATIC", "Trigger:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 454, 184, 48, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                506, 180, 98, 200, IDC_COMBO_PROX_TRIGGER);
            let h_trigger = GetDlgItem(hwnd, IDC_COMBO_PROX_TRIGGER as i32);
            SendMessageW(h_trigger, CB_ADDSTRING, 0, wide("Any player").as_ptr() as LPARAM);
            SendMessageW(h_trigger, CB_ADDSTRING, 0, wide("GM only").as_ptr() as LPARAM);
            SendMessageW(h_trigger, CB_SETCURSEL, pcfg.proximity_watch_only as WPARAM, 0);
            create_control(hwnd, hinstance, font, "STATIC", "On detect:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 218, 62, 20, 0);
            create_control(hwnd, hinstance, font, "COMBOBOX", "",
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0,
                396, 214, 208, 240, IDC_COMBO_PROX_ACTION);
            // "(Press Key)" uses the Key above; otherwise play the chosen recorded sequence.
            let h_action = GetDlgItem(hwnd, IDC_COMBO_PROX_ACTION as i32);
            SendMessageW(h_action, CB_ADDSTRING, 0, wide("(Press Key)").as_ptr() as LPARAM);
            let mut sel_action: usize = 0;
            if let Ok(names) = crate::storage::list_sequences() {
                for (i, name) in names.iter().enumerate() {
                    SendMessageW(h_action, CB_ADDSTRING, 0, wide(name).as_ptr() as LPARAM);
                    if *name == pcfg.proximity_sequence {
                        sel_action = i + 1;
                    }
                }
            }
            SendMessageW(h_action, CB_SETCURSEL, sel_action as WPARAM, 0);
            // Watch patterns used by the "GM only" trigger. Case-insensitive, matched as whole
            // tokens against a player's name, <badge>, nick and guild — one per line or comma-
            // separated. "*" and "?" wildcards are allowed (e.g. "*portal*").
            create_control(hwnd, hinstance, font, "STATIC", "GM names:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 332, 248, 62, 20, 0);
            create_control(hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | WS_VSCROLL
                    | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32,
                0, 396, 244, 208, 48, IDC_EDIT_PROX_WATCH);
            let watch_text = if pcfg.proximity_watch.is_empty() {
                proximity::DEFAULT_WATCH.join(", ")
            } else {
                pcfg.proximity_watch.join(", ")
            };
            SendMessageW(GetDlgItem(hwnd, IDC_EDIT_PROX_WATCH as i32), WM_SETTEXT, 0,
                wide(&watch_text).as_ptr() as LPARAM);
            create_control(hwnd, hinstance, font, "BUTTON", "Detected Players / Ignore\u{2026}",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 332, 300, 200, 26, IDC_BTN_PROX_PLAYERS);
            // Manual update check; the automatic one runs at launch. Slots into the gap left by
            // the Players button on the same row, so the dialog doesn't grow.
            create_control(hwnd, hinstance, font, "BUTTON", "Update",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 536, 300, 68, 26, IDC_BTN_UPDATE);

            // OK button (re-pinned to the window bottom by layout()).
            create_control(hwnd, hinstance, font, "BUTTON", "OK",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0, 285, 470, 70, 28, IDC_BTN_SETTINGS_OK);

            // Pre-fill the monitor X/Y/color fields and the burst rate from config (single fetch).
            // HP/MP/SP share one game window, so the sampled class/title come from HP's config.
            let parent_ptr = GetWindowLongPtrW(GetParent(hwnd), GWLP_USERDATA) as *mut ToolbarControls;
            if !parent_ptr.is_null() {
                let cfg = &(*parent_ptr).config;
                prepopulate_section(hwnd, 0, cfg.hp_monitor_x, cfg.hp_monitor_y, cfg.hp_monitor_color);
                prepopulate_section(hwnd, 1, cfg.mp_monitor_x, cfg.mp_monitor_y, cfg.mp_monitor_color);
                prepopulate_section(hwnd, 2, cfg.sp_monitor_x, cfg.sp_monitor_y, cfg.sp_monitor_color);
                *lock_or_recover(&SAMPLED_CLASS) = cfg.hp_monitor_window_class.clone();
                *lock_or_recover(&SAMPLED_TITLE) = cfg.hp_monitor_window_title.clone();
                SetWindowTextW(GetDlgItem(hwnd, IDC_EDIT_BURST_RATE as i32),
                    wide(&cfg.burst_rate_hz.to_string()).as_ptr());
            }

            layout(hwnd); // size the width-following controls + OK to the initial client area
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

                    // Resolve through the same tiered finder the live monitor uses, so
                    // Sample and the monitor always agree on which window they read.
                    let (sample_hwnd, sample_x, sample_y) = if class.is_empty() {
                        (std::ptr::null_mut(), x, y)
                    } else {
                        (crate::monitor::find_game_window(&class, &title), x, y)
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
            } else if control_id == IDC_BTN_PROX_PLAYERS {
                // Own the Players window by the main toolbar, NOT this transient Settings dialog —
                // otherwise closing Settings destroys it (and the scan view) with it.
                let owner = GetWindow(hwnd, GW_OWNER);
                let owner = if owner.is_null() { hwnd } else { owner };
                super::players::show_players_dialog(owner);
                return 0;
            } else if control_id == IDC_BTN_UPDATE {
                // On-demand check. Runs on a worker thread so a slow/dead network can't freeze the
                // dialog, then hands the answer back here to prompt on the UI thread.
                let toolbar = GetWindow(hwnd, GW_OWNER);
                let toolbar = if toolbar.is_null() { GetParent(hwnd) } else { toolbar };
                let (dlg, tb) = (hwnd as isize, toolbar as isize);
                std::thread::spawn(move || match crate::update::check() {
                    // Hand off to the toolbar so the prompt (and the config write behind
                    // "skip this version") happens on the UI thread, exactly as the
                    // automatic startup check does.
                    Ok(Some(info)) => unsafe {
                        crate::update::set_pending(info);
                        // Latch the gate exactly like the periodic poll does, so a poll
                        // firing now can't stack a second prompt on this one.
                        crate::update::set_prompt_active(true);
                        PostMessageW(tb as HWND, WM_APP_UPDATE_AVAILABLE, 0, 0);
                    },
                    Ok(None) => unsafe {
                        let msg = format!(
                            "You're on the latest version ({}).",
                            crate::update::current_version()
                        );
                        MessageBoxW(dlg as HWND, wide(&msg).as_ptr(),
                            wide("Cadence Update").as_ptr(), MB_OK | MB_ICONINFORMATION);
                    },
                    Err(e) => unsafe {
                        let msg = format!("Couldn't check for updates:\n\n{}", e);
                        MessageBoxW(dlg as HWND, wide(&msg).as_ptr(),
                            wide("Cadence Update").as_ptr(), MB_OK | MB_ICONWARNING);
                    },
                });
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

                            // Read Proximity settings. Arm/disarm is the toolbar "Det" checkbox,
                            // so Settings only edits key/interface/on-detect (not the enabled flag),
                            // and the server IP stays whatever's in config (empty = auto-discover).
                            let pk_idx = SendMessageW(
                                GetDlgItem(hwnd, IDC_COMBO_PROX_KEY as i32), CB_GETCURSEL, 0, 0,
                            ) as usize;
                            let prox_vk = if pk_idx < REMOTE_KEY_OPTIONS.len() { REMOTE_KEY_OPTIONS[pk_idx].0 } else { 0x45 };
                            let if_idx = SendMessageW(
                                GetDlgItem(hwnd, IDC_COMBO_PROX_IFACE as i32), CB_GETCURSEL, 0, 0,
                            );
                            let prox_iface = if if_idx <= 0 {
                                String::new()
                            } else {
                                lock_or_recover(&PROX_DEVICES)
                                    .get(if_idx as usize - 1).cloned().unwrap_or_default()
                            };
                            // On-detect action: "(Press Key)" (index 0) = empty; else the sequence name.
                            let act_combo = GetDlgItem(hwnd, IDC_COMBO_PROX_ACTION as i32);
                            let act_idx = SendMessageW(act_combo, CB_GETCURSEL, 0, 0);
                            let prox_sequence = if act_idx <= 0 {
                                String::new()
                            } else {
                                let len = SendMessageW(act_combo, CB_GETLBTEXTLEN, act_idx as WPARAM, 0);
                                if len > 0 {
                                    let mut buf = vec![0u16; (len as usize) + 1];
                                    SendMessageW(act_combo, CB_GETLBTEXT, act_idx as WPARAM, buf.as_mut_ptr() as LPARAM);
                                    String::from_utf16_lossy(&buf[..len as usize])
                                } else {
                                    String::new()
                                }
                            };
                            let prox_cooldown = (*ptr).config.proximity_cooldown_ms; // vestigial (one-shot)
                            // Trigger mode + watch patterns (comma or newline separated).
                            let prox_watch_only = SendMessageW(
                                GetDlgItem(hwnd, IDC_COMBO_PROX_TRIGGER as i32), CB_GETCURSEL, 0, 0,
                            ) == 1;
                            let mut wbuf = vec![0u16; 2048];
                            let wlen = GetWindowTextW(
                                GetDlgItem(hwnd, IDC_EDIT_PROX_WATCH as i32),
                                wbuf.as_mut_ptr(), wbuf.len() as i32,
                            ) as usize;
                            let prox_watch: Vec<String> = String::from_utf16_lossy(&wbuf[..wlen])
                                .split(|c| c == ',' || c == '\r' || c == '\n')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();

                            (*ptr).config.proximity_vk = prox_vk;
                            (*ptr).config.proximity_iface = prox_iface.clone();
                            (*ptr).config.proximity_sequence = prox_sequence.clone();
                            (*ptr).config.proximity_watch_only = prox_watch_only;
                            (*ptr).config.proximity_watch = prox_watch.clone();
                            // The ignore list is edited in the Players window (live + config cache),
                            // so pull the current value in rather than clobbering it with our stale copy.
                            (*ptr).config.proximity_ignore = proximity::ignored_players();

                            // Push the new settings into the live monitor so a
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

                            // Apply Proximity changes immediately. start() itself waits for any
                            // running capture to stop, so this reliably restarts detection.
                            proximity::set_reaction((*ptr).config.proximity_sequence.clone());
                            // Trigger changes apply live, even to a scan already running.
                            proximity::set_trigger_watch(prox_watch_only);
                            proximity::set_watch_list(prox_watch);
                            proximity::set_watch_gm_flag((*ptr).config.proximity_watch_gm_flag);
                            // Don't disturb an active passive scan (owned by the Players window) —
                            // closing Settings must not kill a running scan.
                            if proximity::is_scan_only() {
                                // leave the scan running
                            } else if (*ptr).config.proximity_enabled {
                                // Det is armed: restart so the new key/interface/on-detect take effect.
                                proximity::set_scan_only(false); // reactive detection
                                proximity::start(prox_vk, prox_iface,
                                    (*ptr).config.proximity_server_ip.clone(), prox_cooldown);
                            } else {
                                proximity::stop();
                            }

                            if let Err(e) = config::save_config(&(*ptr).config) {
                                eprintln!("[Cadence] Config save failed: {}", e);
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
        WM_SIZE => {
            layout(hwnd);
            0
        }
        WM_GETMINMAXINFO => {
            let dpi = dpi_for_window(hwnd);
            let mmi = &mut *(l_param as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = scale(DIALOG_W, dpi);
            mmi.ptMinTrackSize.y = scale(DIALOG_H, dpi);
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
