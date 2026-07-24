mod toolbar;
mod settings;
mod save_dialog;
mod sequences;
mod bind_key;
mod rename_dialog;
mod set_group_dialog;
mod remote;
mod add_binding;
mod players;

pub use toolbar::create_toolbar_window;

use crate::{config, hotkeys, player, recorder, sequence, storage};
use crate::win32_helpers::lock_or_recover;
use std::sync::Mutex;

// Shared state for last recorded events
pub static LAST_EVENTS: Mutex<Option<Vec<sequence::InputEvent>>> = Mutex::new(None);

// Pending events waiting for save dialog
pub(crate) static PENDING_EVENTS: Mutex<Option<Vec<sequence::InputEvent>>> = Mutex::new(None);

// Sequence queue (playlist) - in-memory only
pub(crate) static SEQUENCE_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

// Store selected sequence name for bind-key dialog
pub(crate) static BIND_SEQ_NAME: Mutex<Option<String>> = Mutex::new(None);

// Store selected sequence name for rename dialog
pub(crate) static RENAME_SEQ_NAME: Mutex<Option<String>> = Mutex::new(None);

// Store selected sequence name for set-group dialog
pub(crate) static SET_GROUP_SEQ_NAME: Mutex<Option<String>> = Mutex::new(None);

// Groups that are currently collapsed in the sequences list
pub(crate) static COLLAPSED_GROUPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

// Control IDs - Toolbar
pub(crate) const IDC_BTN_RECORD: u16 = 101;
pub(crate) const IDC_BTN_PLAY: u16 = 102;
pub(crate) const IDC_CHK_LOOP: u16 = 103;
pub(crate) const IDC_CHK_TOPMOST: u16 = 104;
pub(crate) const IDC_BTN_SETTINGS: u16 = 105;
pub(crate) const IDC_STATUS: u16 = 106;
pub(crate) const IDC_BTN_SEQUENCES: u16 = 107;
pub(crate) const IDC_BTN_REMOTE: u16 = 108;
pub(crate) const IDC_CHK_PET: u16 = 109;
pub(crate) const IDC_CHK_HP: u16 = 110;
pub(crate) const IDC_BTN_BURST: u16 = 111;
pub(crate) const IDC_CHK_MP: u16 = 113;
pub(crate) const IDC_CHK_SP: u16 = 114;
pub(crate) const IDC_CHK_DET: u16 = 115;

/// Posted to the toolbar window when proximity detection fires, so it can uncheck "Det" (one-shot).
pub(crate) const WM_APP_PROXIMITY_HIT: u32 = winapi::um::winuser::WM_APP + 3;

// Settings dialog controls
pub(crate) const IDC_COMBO_RECORD_KEY: u16 = 201;
pub(crate) const IDC_COMBO_PLAY_KEY: u16 = 202;
pub(crate) const IDC_BTN_SETTINGS_OK: u16 = 203;
pub(crate) const IDC_COMBO_QUEUE_KEY: u16 = 204;
pub(crate) const IDC_EDIT_HP_X: u16 = 205;
pub(crate) const IDC_EDIT_HP_Y: u16 = 206;
pub(crate) const IDC_BTN_HP_SAMPLE: u16 = 207;
pub(crate) const IDC_STATIC_HP_COLOR: u16 = 208;
pub(crate) const IDC_BTN_HP_PICK: u16 = 209;
pub(crate) const IDC_STATIC_HP_LIVE: u16 = 210;
pub(crate) const IDC_EDIT_MP_X: u16 = 220;
pub(crate) const IDC_EDIT_MP_Y: u16 = 221;
pub(crate) const IDC_BTN_MP_SAMPLE: u16 = 222;
pub(crate) const IDC_STATIC_MP_COLOR: u16 = 223;
pub(crate) const IDC_BTN_MP_PICK: u16 = 224;
pub(crate) const IDC_STATIC_MP_LIVE: u16 = 225;
pub(crate) const IDC_EDIT_SP_X: u16 = 230;
pub(crate) const IDC_EDIT_SP_Y: u16 = 231;
pub(crate) const IDC_BTN_SP_SAMPLE: u16 = 232;
pub(crate) const IDC_STATIC_SP_COLOR: u16 = 233;
pub(crate) const IDC_BTN_SP_PICK: u16 = 234;
pub(crate) const IDC_STATIC_SP_LIVE: u16 = 235;
pub(crate) const IDC_COMBO_BURST_KEY: u16 = 211;
pub(crate) const IDC_EDIT_BURST_RATE: u16 = 212;
pub(crate) const IDC_COMBO_PROX_KEY: u16 = 241;
pub(crate) const IDC_COMBO_PROX_IFACE: u16 = 242;
pub(crate) const IDC_BTN_PROX_PLAYERS: u16 = 245;
pub(crate) const IDC_COMBO_PROX_ACTION: u16 = 246;
// Section dividers (given IDs so the resize reflow can stretch them to the window width).
pub(crate) const IDC_SETTINGS_DIV1: u16 = 250;
pub(crate) const IDC_SETTINGS_DIV2: u16 = 251;
pub(crate) const IDC_SETTINGS_DIV3: u16 = 252;

// Players (detected/ignore) dialog controls
pub(crate) const IDC_LIST_PLAYERS: u16 = 901;
pub(crate) const IDC_BTN_PLAYER_TOGGLE: u16 = 902;
pub(crate) const IDC_BTN_PLAYER_CLEAR: u16 = 903;
pub(crate) const IDC_BTN_PLAYER_CLOSE: u16 = 904;
pub(crate) const IDC_BTN_PLAYER_DELETE: u16 = 905;
pub(crate) const IDC_CHK_PLAYER_SCAN: u16 = 907;   // passive scan on/off
pub(crate) const IDC_RADIO_SESSION: u16 = 908;     // view: this session
pub(crate) const IDC_RADIO_ALLTIME: u16 = 909;     // view: all-time (persistent)
pub(crate) const IDC_EDIT_PLAYER_SEARCH: u16 = 910; // filter box
pub(crate) const IDC_HDR_PLAYERS: u16 = 911;       // monospace column header (id so it can reflow)
pub(crate) const IDC_CMB_PLAYER_CLASS: u16 = 912;  // filter: class
pub(crate) const IDC_CMB_PLAYER_SCHOOL: u16 = 913; // filter: school
pub(crate) const IDC_CMB_PLAYER_MAP: u16 = 914;    // filter: map (populated from data)
pub(crate) const IDC_CMB_PLAYER_GUILD: u16 = 915;  // filter: guild (populated from data)
pub(crate) const IDC_BTN_PLAYER_EXPORT: u16 = 906; // export current view to .xlsx

// Save dialog controls
pub(crate) const IDC_EDIT_SEQ_NAME: u16 = 301;
pub(crate) const IDC_BTN_SAVE_OK: u16 = 302;
pub(crate) const IDC_BTN_SAVE_CANCEL: u16 = 303;

// Sequence manager controls
pub(crate) const IDC_LIST_SEQUENCES: u16 = 401;
pub(crate) const IDC_BTN_BIND_KEY: u16 = 402;
pub(crate) const IDC_BTN_DELETE_SEQ: u16 = 403;
pub(crate) const IDC_BTN_PLAY_SEQ: u16 = 404;
pub(crate) const IDC_BTN_SET_DEFAULT: u16 = 405;
pub(crate) const IDC_BTN_RENAME_SEQ: u16 = 406;
pub(crate) const IDC_BTN_SET_GROUP: u16 = 407;

// Queue controls
pub(crate) const IDC_BTN_QUEUE_ADD: u16 = 601;
pub(crate) const IDC_BTN_QUEUE_REMOVE: u16 = 602;
pub(crate) const IDC_LIST_QUEUE: u16 = 603;
pub(crate) const IDC_BTN_QUEUE_UP: u16 = 604;
pub(crate) const IDC_BTN_QUEUE_DOWN: u16 = 605;
pub(crate) const IDC_BTN_PLAY_QUEUE: u16 = 606;
pub(crate) const IDC_CHK_SHUFFLE: u16 = 607;

// Bind key dialog controls
pub(crate) const IDC_COMBO_BIND_KEY: u16 = 501;
pub(crate) const IDC_BTN_BIND_OK: u16 = 502;
pub(crate) const IDC_BTN_BIND_CANCEL: u16 = 503;

// Remote control dialog controls
pub(crate) const IDC_EDIT_RECV_PORT: u16 = 701;
pub(crate) const IDC_EDIT_RECV_PASSWORD: u16 = 702;
pub(crate) const IDC_BTN_RECV_TOGGLE: u16 = 703;
pub(crate) const IDC_STATIC_RECV_STATUS: u16 = 704;
pub(crate) const IDC_CHK_AUTO_LISTEN: u16 = 705;
pub(crate) const IDC_LIST_SEND_HOSTS: u16 = 720;
pub(crate) const IDC_EDIT_ADD_HOST: u16 = 721;
pub(crate) const IDC_BTN_ADD_HOST: u16 = 722;
pub(crate) const IDC_BTN_REMOVE_HOST: u16 = 723;
pub(crate) const IDC_EDIT_SEND_PORT: u16 = 712;
pub(crate) const IDC_EDIT_SEND_PASSWORD: u16 = 713;
pub(crate) const IDC_EDIT_SEND_CODE: u16 = 714;
pub(crate) const IDC_BTN_SEND_PLAY: u16 = 715;
pub(crate) const IDC_BTN_SEND_QUEUE: u16 = 716;
pub(crate) const IDC_BTN_SEND_STOP: u16 = 717;
pub(crate) const IDC_STATIC_SEND_STATUS: u16 = 718;

// Remote hotkey binding controls
pub(crate) const IDC_LIST_REMOTE_BINDINGS: u16 = 801;
pub(crate) const IDC_BTN_ADD_BINDING: u16 = 802;
pub(crate) const IDC_BTN_REMOVE_BINDING: u16 = 803;

// Add binding dialog controls
pub(crate) const IDC_CHK_MOD_ALT: u16 = 851;
pub(crate) const IDC_CHK_MOD_CTRL: u16 = 852;
pub(crate) const IDC_CHK_MOD_SHIFT: u16 = 853;
pub(crate) const IDC_COMBO_BIND_VK: u16 = 854;
pub(crate) const IDC_EDIT_BIND_SEQ: u16 = 855;
pub(crate) const IDC_BTN_BIND_ADD_OK: u16 = 856;
pub(crate) const IDC_BTN_BIND_ADD_CANCEL: u16 = 857;

// Timer IDs
pub(crate) const TIMER_STATUS: usize = 1;
pub(crate) const TIMER_REMOTE: usize = 2;
pub(crate) const TIMER_HP_PICK: usize = 3;
pub(crate) const TIMER_PLAYERS: usize = 4;

pub fn handle_record_toggle() {
    if recorder::is_recording() {
        if let Some(events) = recorder::stop_recording() {
            if !events.is_empty() {
                *lock_or_recover(&LAST_EVENTS) = Some(events.clone());
                *lock_or_recover(&PENDING_EVENTS) = Some(events);
                unsafe { save_dialog::show_save_dialog(); }
            }
        }
    } else if player::is_playing() {
        // Cannot record while playing
    } else {
        recorder::start_recording();
    }
}

pub fn handle_play_toggle() {
    if player::is_playing() {
        player::cancel_playback();
    } else if recorder::is_recording() {
        // Cannot play while recording
    } else {
        let queue = lock_or_recover(&SEQUENCE_QUEUE).clone();
        if !queue.is_empty() {
            let mut event_lists = Vec::new();
            for name in &queue {
                if let Ok(seq) = storage::load_sequence(name) {
                    event_lists.push(seq.events);
                }
            }
            if !event_lists.is_empty() {
                player::play_queue(event_lists);
            }
        } else {
            // Try default sequence from config, then fall back to LAST_EVENTS
            let cfg = config::load_config();
            if let Some(ref name) = cfg.default_sequence {
                if let Ok(seq) = storage::load_sequence(name) {
                    player::play_sequence(seq.events);
                    return;
                }
            }
            let events = lock_or_recover(&LAST_EVENTS);
            if let Some(ref evts) = *events {
                player::play_sequence(evts.clone());
            }
        }
    }
}

pub fn handle_play_queue_hotkey() {
    if player::is_playing() || recorder::is_recording() {
        return;
    }
    let queue = lock_or_recover(&SEQUENCE_QUEUE).clone();
    if queue.is_empty() {
        return;
    }
    let mut event_lists = Vec::new();
    for name in &queue {
        if let Ok(seq) = storage::load_sequence(name) {
            event_lists.push(seq.events);
        }
    }
    if !event_lists.is_empty() {
        player::play_queue(event_lists);
    }
}

pub fn handle_play_sequence(vk: u16) {
    if player::is_playing() || recorder::is_recording() {
        return;
    }
    if let Some(seq_name) = hotkeys::sequence_for_vk(vk) {
        if let Ok(seq) = storage::load_sequence(&seq_name) {
            player::play_sequence(seq.events);
        }
    }
}

/// Load all sequence hotkey bindings from disk.
pub fn load_all_bindings() -> Vec<(u16, String)> {
    let mut bindings = Vec::new();
    if let Ok(names) = storage::list_sequences() {
        for name in names {
            if let Ok(seq) = storage::load_sequence(&name) {
                if let Some(hk) = seq.hotkey {
                    bindings.push((hk.vk_code, seq.name));
                }
            }
        }
    }
    bindings
}

/// Rebuild and apply sequence bindings from disk.
pub(crate) fn refresh_bindings() {
    let bindings = load_all_bindings();
    hotkeys::set_sequence_bindings(bindings);
}
