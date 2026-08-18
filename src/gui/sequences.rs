use crate::win32_helpers::{wide, create_control, register_and_create_dialog, lock_or_recover, dpi_for_window, scaled_font, vk_name};
use std::collections::BTreeMap;
use crate::{config, hotkeys, player, recorder, sequence, storage};
use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

static SEQUENCES_HWND: AtomicIsize = AtomicIsize::new(0);

// Sequence name per list row, so selection never has to parse names back out of decorated
// display text (hotkey suffixes, last/new markers). A row's LB_ITEMDATA is its 1-based index
// into this vec; group headers keep item data 0.
static ROW_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub unsafe fn show_sequences_window(parent: HWND) {
    let existing = SEQUENCES_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }

    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());

    // Position near parent
    let mut parent_rect: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut parent_rect);

    let sx = parent_rect.left;
    let sy = parent_rect.bottom + 4;

    let hwnd = register_and_create_dialog(
        "CadenceList", "Items",
        sequences_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        sx, sy, 560, 455,
        parent, hinstance,
    );
    SEQUENCES_HWND.store(hwnd as isize, Ordering::Release);
}

/// Refresh the sequences list if the window is open.
pub unsafe fn refresh_sequences_list() {
    let hwnd = SEQUENCES_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
        if !h_list.is_null() {
            SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
            populate_sequences_list(h_list);
        }
    }
}

unsafe extern "system" fn sequences_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let font = scaled_font(dpi_for_window(hwnd));

            // -- Left panel: Saved Sequences --
            create_control(
                hwnd, hinstance, font, "STATIC", "Saved Sequences",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                10, 2, 200, 16, 0,
            );

            // Extended selection: click, Shift+click for ranges, Ctrl+click to toggle —
            // so Delete / >> / Set Group / Play can act on many sequences at once.
            let h_list = create_control(
                hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_EXTENDEDSEL as u32,
                WS_EX_CLIENTEDGE as u32,
                10, 20, 220, 290, IDC_LIST_SEQUENCES,
            );
            populate_sequences_list(h_list);

            // Row under the list: arrange a sequence inside its group, and group-level actions.
            create_control(
                hwnd, hinstance, font, "BUTTON", "Up",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                10, 314, 44, 26, IDC_BTN_SEQ_UP,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Down",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                58, 314, 50, 26, IDC_BTN_SEQ_DOWN,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Select All",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                112, 314, 74, 26, IDC_BTN_SELECT_ALL,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Group >>",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                190, 314, 78, 26, IDC_BTN_QUEUE_ADD_GROUP,
            );

            // -- Transfer buttons (between panels) --
            create_control(
                hwnd, hinstance, font, "BUTTON", ">>",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                237, 105, 42, 30, IDC_BTN_QUEUE_ADD,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "<<",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                237, 145, 42, 30, IDC_BTN_QUEUE_REMOVE,
            );

            // -- Right panel: Queue --
            create_control(
                hwnd, hinstance, font, "STATIC", "Queue",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                286, 2, 220, 16, IDC_STATIC_QUEUE_TOTAL,
            );

            create_control(
                hwnd, hinstance, font, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_EXTENDEDSEL as u32,
                WS_EX_CLIENTEDGE as u32,
                286, 20, 220, 290, IDC_LIST_QUEUE,
            );
            refresh_queue_list(hwnd);

            // Queue reorder buttons
            create_control(
                hwnd, hinstance, font, "BUTTON", "Up",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                286, 314, 55, 26, IDC_BTN_QUEUE_UP,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Down",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                346, 314, 55, 26, IDC_BTN_QUEUE_DOWN,
            );

            // -- Bottom row: action buttons --
            create_control(
                hwnd, hinstance, font, "BUTTON", "Bind Key",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                10, 350, 80, 30, IDC_BTN_BIND_KEY,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Delete",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                96, 350, 64, 30, IDC_BTN_DELETE_SEQ,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Play",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                166, 350, 44, 30, IDC_BTN_PLAY_SEQ,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Default",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                214, 350, 54, 30, IDC_BTN_SET_DEFAULT,
            );

            // Second row: Rename, Set Group, Duplicate
            create_control(
                hwnd, hinstance, font, "BUTTON", "Rename",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                10, 385, 80, 30, IDC_BTN_RENAME_SEQ,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Set Group",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                96, 385, 80, 30, IDC_BTN_SET_GROUP,
            );

            create_control(
                hwnd, hinstance, font, "BUTTON", "Duplicate",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                182, 385, 86, 30, IDC_BTN_DUPLICATE_SEQ,
            );

            // Shuffle checkbox
            let chk_shuffle = create_control(
                hwnd, hinstance, font, "BUTTON", "Shuffle",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                286, 350, 70, 30, IDC_CHK_SHUFFLE,
            );
            if player::is_shuffle_mode() {
                SendMessageW(chk_shuffle, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
            }

            create_control(
                hwnd, hinstance, font, "BUTTON", "Play Queue",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                400, 350, 104, 30, IDC_BTN_PLAY_QUEUE,
            );
            // Named, persisted queues (repeats allowed) — save the current one / load one back.
            create_control(
                hwnd, hinstance, font, "BUTTON", "Saved Queues\u{2026}",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                286, 385, 220, 30, IDC_BTN_QUEUE_SAVED,
            );

            0
        }
        WM_APP_QUEUE_CHANGED => {
            refresh_queue_list(hwnd);
            0
        }
        WM_COMMAND => {
            let control_id = LOWORD(w_param as u32);
            match control_id {
                x if x == IDC_BTN_BIND_KEY => handle_bind_key(hwnd),
                x if x == IDC_BTN_DELETE_SEQ => handle_delete_seq(hwnd),
                x if x == IDC_BTN_PLAY_SEQ => handle_play_seq(hwnd),
                x if x == IDC_BTN_QUEUE_ADD => handle_queue_add(hwnd),
                x if x == IDC_BTN_QUEUE_REMOVE => handle_queue_remove(hwnd),
                x if x == IDC_BTN_QUEUE_UP => handle_queue_up(hwnd),
                x if x == IDC_BTN_QUEUE_DOWN => handle_queue_down(hwnd),
                x if x == IDC_BTN_SET_DEFAULT => handle_set_default(hwnd),
                x if x == IDC_BTN_RENAME_SEQ => handle_rename_seq(hwnd),
                x if x == IDC_BTN_SET_GROUP => handle_set_group(hwnd),
                x if x == IDC_BTN_DUPLICATE_SEQ => handle_duplicate(hwnd),
                x if x == IDC_BTN_SEQ_UP => handle_seq_move(hwnd, -1),
                x if x == IDC_BTN_SEQ_DOWN => handle_seq_move(hwnd, 1),
                x if x == IDC_BTN_QUEUE_ADD_GROUP => handle_queue_add_group(hwnd),
                x if x == IDC_BTN_SELECT_ALL => {
                    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
                    // Select every row; headers get filtered out by the bulk operations.
                    SendMessageW(h_list, LB_SETSEL, 1, -1isize);
                }
                x if x == IDC_LIST_SEQUENCES => {
                    if HIWORD(w_param as u32) == LBN_SELCHANGE {
                        handle_list_click(hwnd);
                    }
                }
                x if x == IDC_BTN_PLAY_QUEUE => handle_play_queue(),
                x if x == IDC_BTN_QUEUE_SAVED => saved_queues_dialog::show_saved_queues_dialog(hwnd),
                x if x == IDC_CHK_SHUFFLE => {
                    let h_chk = GetDlgItem(hwnd, IDC_CHK_SHUFFLE as i32);
                    let checked = SendMessageW(h_chk, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
                    player::set_shuffle_mode(checked);
                    let mut cfg = config::load_config();
                    cfg.shuffle_queue = checked;
                    let _ = config::save_config(&cfg);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            SEQUENCES_HWND.store(0, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

// ---- Command handlers ----

unsafe fn handle_bind_key(hwnd: HWND) {
    if let Some(name) = get_selected_sequence_name(hwnd) {
        *lock_or_recover(&BIND_SEQ_NAME) = Some(name);
        bind_key::show_bind_key_dialog(hwnd);
    }
}

unsafe fn handle_delete_seq(hwnd: HWND) {
    let names = get_selected_sequence_names(hwnd);
    if names.is_empty() {
        return;
    }
    // One confirmation for the whole selection; list a handful of names, then a count.
    let shown: Vec<&str> = names.iter().take(8).map(|n| n.as_str()).collect();
    let listing = if names.len() > shown.len() {
        format!("{}\n\u{2026}and {} more", shown.join("\n"), names.len() - shown.len())
    } else {
        shown.join("\n")
    };
    let prompt = if names.len() == 1 {
        format!("Delete \"{}\"?", names[0])
    } else {
        format!("Delete these {} sequences?\n\n{}", names.len(), listing)
    };
    let result = MessageBoxW(hwnd, wide(&prompt).as_ptr(), wide("Confirm Delete").as_ptr(),
        MB_YESNO | MB_ICONQUESTION);
    if result != IDYES {
        return;
    }
    let mut cfg = config::load_config();
    let mut cfg_changed = false;
    for name in &names {
        if let Err(e) = storage::delete_sequence(name) {
            eprintln!("[Cadence] Failed to delete sequence: {}", e);
        }
        // Clear default sequence if it was among the deleted
        if cfg.default_sequence.as_deref() == Some(name.as_str()) {
            cfg.default_sequence = None;
            cfg_changed = true;
        }
        // Forget the last-played memory if it named a deleted sequence.
        if storage::last_played().name == *name {
            storage::set_last_played(String::new(), Vec::new());
        }
    }
    if cfg_changed {
        let _ = config::save_config(&cfg);
    }
    refresh_bindings();
    edit_queue(|q| q.retain(|n| !names.contains(n)));
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    populate_sequences_list(h_list);
    refresh_queue_list(hwnd);
}

unsafe fn handle_set_default(hwnd: HWND) {
    if let Some(name) = get_selected_sequence_name(hwnd) {
        // Save as default sequence in config
        let mut cfg = config::load_config();
        cfg.default_sequence = Some(name.clone());
        let _ = config::save_config(&cfg);

        // Also load into LAST_EVENTS for immediate F11 use
        if let Ok(seq) = storage::load_sequence(&name) {
            *lock_or_recover(&LAST_EVENTS) = Some(seq.events);
        }
    }
}

unsafe fn handle_rename_seq(hwnd: HWND) {
    if let Some(name) = get_selected_sequence_name(hwnd) {
        *lock_or_recover(&RENAME_SEQ_NAME) = Some(name);
        rename_dialog::show_rename_dialog(hwnd);
    }
}

unsafe fn handle_set_group(hwnd: HWND) {
    let names = get_selected_sequence_names(hwnd);
    if !names.is_empty() {
        *lock_or_recover(&SET_GROUP_SEQ_NAMES) = names;
        set_group_dialog::show_set_group_dialog(hwnd);
    }
}

unsafe fn handle_duplicate(hwnd: HWND) {
    let names = get_selected_sequence_names(hwnd);
    if !names.is_empty() {
        *lock_or_recover(&DUPLICATE_SEQ_NAMES) = names;
        duplicate_dialog::show_duplicate_dialog(hwnd);
    }
}

/// Move the single selected sequence one slot up (`delta` = -1) or down (+1) inside its
/// group, and persist the group's new order.
unsafe fn handle_seq_move(hwnd: HWND, delta: i32) {
    let Some(name) = get_selected_sequence_name(hwnd) else { return };
    let group = storage::load_sequence(&name).ok().and_then(|s| s.group);
    let mut members = storage::group_members(group.as_deref());
    let Some(idx) = members.iter().position(|n| *n == name) else { return };
    let target = idx as i32 + delta;
    if target < 0 || target as usize >= members.len() {
        return;
    }
    members.swap(idx, target as usize);
    if let Err(e) = storage::set_group_order(&members) {
        eprintln!("[Cadence] Failed to save group order: {}", e);
    }
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    populate_sequences_list(h_list);
    select_seq_row_by_name(h_list, &name);
}

/// Append every member of each selected group to the queue, in the group's saved order.
/// A group counts as selected if its header row is (works while collapsed) or any of its
/// sequence rows is.
unsafe fn handle_queue_add_group(hwnd: HWND) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
    let count = SendMessageW(h_list, LB_GETSELCOUNT, 0, 0);
    if count <= 0 {
        return;
    }
    let mut idxs = vec![0i32; count as usize];
    let got = SendMessageW(h_list, LB_GETSELITEMS, count as WPARAM, idxs.as_mut_ptr() as LPARAM);
    if got <= 0 {
        return;
    }
    let meta = storage::list_sequence_meta().unwrap_or_default();
    let mut groups: Vec<Option<String>> = Vec::new();
    {
        let rows = lock_or_recover(&ROW_NAMES);
        for &idx in &idxs[..got as usize] {
            let data = SendMessageW(h_list, LB_GETITEMDATA, idx as WPARAM, 0);
            let group = if data == 0 {
                match header_group_key(h_list, idx as isize) {
                    Some(key) if key == UNGROUPED_HEADER => None,
                    Some(key) => Some(key),
                    None => continue,
                }
            } else {
                let Some(name) = rows.get(data as usize - 1) else { continue };
                match meta.iter().find(|m| m.name == *name) {
                    Some(m) => m.group.clone(),
                    None => continue,
                }
            };
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
    }
    if groups.is_empty() {
        return;
    }
    edit_queue(|queue| {
        for group in &groups {
            queue.extend(storage::group_members(group.as_deref()));
        }
    });
    refresh_queue_list(hwnd);
}

const UNGROUPED_HEADER: &str = "(Ungrouped)";

/// Group name of a header row, i.e. its text minus the "[+] " / "[-] " prefix.
unsafe fn header_group_key(h_list: HWND, idx: isize) -> Option<String> {
    let len = SendMessageW(h_list, LB_GETTEXTLEN, idx as WPARAM, 0);
    if len < 4 {
        return None;
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    SendMessageW(h_list, LB_GETTEXT, idx as WPARAM, buf.as_mut_ptr() as LPARAM);
    let text = String::from_utf16_lossy(&buf[..len as usize]);
    text.get(4..).map(|s| s.to_string())
}

/// Select (and caret) the row showing `name`, if it is visible.
unsafe fn select_seq_row_by_name(h_list: HWND, name: &str) {
    let rows = lock_or_recover(&ROW_NAMES);
    let count = SendMessageW(h_list, LB_GETCOUNT, 0, 0);
    for idx in 0..count {
        let data = SendMessageW(h_list, LB_GETITEMDATA, idx as WPARAM, 0);
        if data > 0 && rows.get(data as usize - 1).is_some_and(|n| n == name) {
            SendMessageW(h_list, LB_SETSEL, 0, -1isize);
            SendMessageW(h_list, LB_SETSEL, 1, idx as LPARAM);
            SendMessageW(h_list, LB_SETCARETINDEX, idx as WPARAM, 0);
            return;
        }
    }
}

unsafe fn handle_list_click(hwnd: HWND) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
    // Caret = the item actually clicked; LB_GETCURSEL is not meaningful on an
    // extended-selection list box.
    let idx = SendMessageW(h_list, LB_GETCARETINDEX, 0, 0);
    if idx < 0 { return; }

    // Only act on header items (data = 0); sequence items (data = 1) are ignored
    if SendMessageW(h_list, LB_GETITEMDATA, idx as WPARAM, 0) != 0 { return; }

    let Some(group_key) = header_group_key(h_list, idx) else { return };

    {
        let mut collapsed = lock_or_recover(&COLLAPSED_GROUPS);
        if let Some(i) = collapsed.iter().position(|g| g == &group_key) {
            collapsed.remove(i);
        } else {
            collapsed.push(group_key);
        }
    }

    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    populate_sequences_list(h_list);
}

unsafe fn handle_play_seq(hwnd: HWND) {
    let names = get_selected_sequence_names(hwnd);
    if names.is_empty() || player::is_playing() || recorder::is_recording() {
        return;
    }
    if names.len() == 1 {
        if let Ok(seq) = storage::load_sequence(&names[0]) {
            *lock_or_recover(&LAST_EVENTS) = Some(seq.events.clone());
            player::set_source(player::PlaybackSource::Sequence(names[0].clone()));
            player::play_sequence(seq.events);
            refresh_sequences_list(); // move the "last played" marker immediately
        }
    } else {
        // Several selected: play them back-to-back as an ad-hoc queue, in list order.
        let mut event_lists = Vec::new();
        for name in &names {
            if let Ok(seq) = storage::load_sequence(name) {
                event_lists.push(seq.events);
            }
        }
        if !event_lists.is_empty() {
            player::set_source(player::PlaybackSource::Queue(names));
            player::play_queue(event_lists);
            refresh_sequences_list();
        }
    }
}

unsafe fn handle_queue_add(hwnd: HWND) {
    let names = get_selected_sequence_names(hwnd);
    if !names.is_empty() {
        edit_queue(|q| q.extend(names));
        refresh_queue_list(hwnd);
    }
}

unsafe fn handle_queue_remove(hwnd: HWND) {
    let h_queue = GetDlgItem(hwnd, IDC_LIST_QUEUE as i32);
    let count = SendMessageW(h_queue, LB_GETSELCOUNT, 0, 0);
    if count <= 0 {
        return;
    }
    let mut idxs = vec![0i32; count as usize];
    let got = SendMessageW(h_queue, LB_GETSELITEMS, count as WPARAM, idxs.as_mut_ptr() as LPARAM);
    if got <= 0 {
        return;
    }
    // Remove back-to-front so earlier indices stay valid.
    idxs[..got as usize].sort_unstable_by(|a, b| b.cmp(a));
    edit_queue(|queue| {
        for &idx in &idxs[..got as usize] {
            if (idx as usize) < queue.len() {
                queue.remove(idx as usize);
            }
        }
    });
    refresh_queue_list(hwnd);
}

/// Re-select one row after a reorder (extended-sel lists use LB_SETSEL, not LB_SETCURSEL).
unsafe fn select_queue_row(h_queue: HWND, idx: usize) {
    SendMessageW(h_queue, LB_SETSEL, 0, -1isize); // clear
    SendMessageW(h_queue, LB_SETSEL, 1, idx as LPARAM);
    SendMessageW(h_queue, LB_SETCARETINDEX, idx as WPARAM, 0);
}

unsafe fn handle_queue_up(hwnd: HWND) {
    let h_queue = GetDlgItem(hwnd, IDC_LIST_QUEUE as i32);
    let idx = SendMessageW(h_queue, LB_GETCARETINDEX, 0, 0);
    if idx > 0 {
        let idx = idx as usize;
        edit_queue(|queue| {
            if idx < queue.len() {
                queue.swap(idx, idx - 1);
            }
        });
        refresh_queue_list(hwnd);
        select_queue_row(h_queue, idx - 1);
    }
}

unsafe fn handle_queue_down(hwnd: HWND) {
    let h_queue = GetDlgItem(hwnd, IDC_LIST_QUEUE as i32);
    let idx = SendMessageW(h_queue, LB_GETCARETINDEX, 0, 0);
    if idx >= 0 {
        let idx = idx as usize;
        let swapped = edit_queue(|queue| {
            if idx + 1 < queue.len() {
                queue.swap(idx, idx + 1);
                true
            } else {
                false
            }
        });
        if swapped {
            refresh_queue_list(hwnd);
            select_queue_row(h_queue, idx + 1);
        }
    }
}

unsafe fn handle_play_queue() {
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
        // Same as the queue hotkey / remote PLAY_QUEUE: name the source so the status line,
        // last-played memory and the update-resume marker describe this queue.
        player::set_source(player::PlaybackSource::Queue(queue));
        player::play_queue(event_lists);
    }
}

/// The queue was replaced from another thread/window (saved queue loaded, remote PLAY_LIST):
/// re-list it if the Items window is open. Posted, so it's safe off the UI thread.
pub(crate) fn notify_queue_changed() {
    let hwnd = SEQUENCES_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        unsafe {
            PostMessageW(hwnd, WM_APP_QUEUE_CHANGED, 0, 0);
        }
    }
}

// ---- Helpers ----

/// Get the raw sequence name from the selected listbox item. Returns None for group headers.
/// All selected sequence rows in list order (group headers filtered out).
unsafe fn get_selected_sequence_names(hwnd: HWND) -> Vec<String> {
    let h_list = GetDlgItem(hwnd, IDC_LIST_SEQUENCES as i32);
    let count = SendMessageW(h_list, LB_GETSELCOUNT, 0, 0);
    if count <= 0 {
        return Vec::new();
    }
    let mut idxs = vec![0i32; count as usize];
    let got = SendMessageW(h_list, LB_GETSELITEMS, count as WPARAM, idxs.as_mut_ptr() as LPARAM);
    if got <= 0 {
        return Vec::new();
    }
    // Group headers carry item data 0; sequence rows carry their 1-based ROW_NAMES index,
    // so names come back exact no matter how the display text is decorated.
    let rows = lock_or_recover(&ROW_NAMES);
    idxs[..got as usize]
        .iter()
        .filter_map(|&idx| {
            let data = SendMessageW(h_list, LB_GETITEMDATA, idx as WPARAM, 0);
            if data > 0 { rows.get(data as usize - 1).cloned() } else { None }
        })
        .collect()
}

/// The single selected sequence, for one-target actions (Bind Key, Rename, Default).
/// Tells the user off instead of guessing when several are selected.
unsafe fn get_selected_sequence_name(hwnd: HWND) -> Option<String> {
    let mut names = get_selected_sequence_names(hwnd);
    match names.len() {
        1 => names.pop(),
        0 => None,
        _ => {
            MessageBoxW(hwnd, wide("Select a single sequence for this action.").as_ptr(),
                wide("Cadence").as_ptr(), MB_OK | MB_ICONINFORMATION);
            None
        }
    }
}

/// Add one sequence row: display text decorated freely, real name kept in ROW_NAMES and
/// referenced from the row's item data (1-based; 0 marks a group header).
unsafe fn add_seq_row(h_list: HWND, display: &str, name: &str, rows: &mut Vec<String>) {
    rows.push(name.to_string());
    let ws = wide(display);
    let ii = SendMessageW(h_list, LB_ADDSTRING, 0, ws.as_ptr() as LPARAM);
    SendMessageW(h_list, LB_SETITEMDATA, ii as WPARAM, rows.len() as LPARAM);
}

unsafe fn populate_sequences_list(h_list: HWND) {
    if let Ok(items) = storage::list_sequence_meta() {
        let bindings = hotkeys::current_sequence_bindings();
        let collapsed = lock_or_recover(&COLLAPSED_GROUPS);
        let last_played = storage::last_played().name;
        let newest = storage::newest_sequence();
        let mut rows: Vec<String> = Vec::new();

        // Bucket by group, then order each bucket by its saved arrangement so the list shows
        // (and >> / Play / Group >> hand out) sequences in play order.
        let mut grouped: BTreeMap<String, Vec<storage::SeqMeta>> = BTreeMap::new();
        let mut ungrouped: Vec<storage::SeqMeta> = Vec::new();

        for item in items {
            match &item.group {
                Some(g) => grouped.entry(g.clone()).or_default().push(item),
                None => ungrouped.push(item),
            }
        }
        for members in grouped.values_mut() {
            storage::sort_group_members(members);
        }
        storage::sort_group_members(&mut ungrouped);

        let has_groups = !grouped.is_empty();

        for (group_name, members) in &grouped {
            let is_collapsed = collapsed.iter().any(|g| g == group_name);
            let prefix = if is_collapsed { "[+]" } else { "[-]" };
            let header = wide(&format!("{} {}", prefix, group_name));
            let hi = SendMessageW(h_list, LB_ADDSTRING, 0, header.as_ptr() as LPARAM);
            SendMessageW(h_list, LB_SETITEMDATA, hi as WPARAM, 0isize as LPARAM);

            if !is_collapsed {
                for m in members {
                    let display =
                        format_seq_display(m, &bindings, true, &last_played, newest.as_deref());
                    add_seq_row(h_list, &display, &m.name, &mut rows);
                }
            }
        }

        if has_groups && !ungrouped.is_empty() {
            let is_collapsed = collapsed.iter().any(|g| g == UNGROUPED_HEADER);
            let prefix = if is_collapsed { "[+]" } else { "[-]" };
            let header = wide(&format!("{} {}", prefix, UNGROUPED_HEADER));
            let hi = SendMessageW(h_list, LB_ADDSTRING, 0, header.as_ptr() as LPARAM);
            SendMessageW(h_list, LB_SETITEMDATA, hi as WPARAM, 0isize as LPARAM);

            if !is_collapsed {
                for m in &ungrouped {
                    let display =
                        format_seq_display(m, &bindings, false, &last_played, newest.as_deref());
                    add_seq_row(h_list, &display, &m.name, &mut rows);
                }
            }
        } else {
            for m in &ungrouped {
                let display =
                    format_seq_display(m, &bindings, false, &last_played, newest.as_deref());
                add_seq_row(h_list, &display, &m.name, &mut rows);
            }
        }

        *lock_or_recover(&ROW_NAMES) = rows;
    }
}

fn format_seq_display(
    meta: &storage::SeqMeta,
    bindings: &[(u16, String)],
    indent: bool,
    last_played: &str,
    newest: Option<&str>,
) -> String {
    let name = meta.name.as_str();
    let mut base = format!("{} ({})", name, sequence::format_duration(meta.duration_micros));
    if let Some((vk, _)) = bindings.iter().find(|(_, n)| n == name) {
        base.push_str(&format!(" [{}]", vk_name(*vk)));
    }
    // "Which one did I run?" / "which one did I just record?" — answered in the list itself.
    if name == last_played {
        base.push_str("  \u{25C0} last played");
    }
    if newest == Some(name) {
        base.push_str("  \u{2605} new");
    }
    if indent { format!("  {}", base) } else { base }
}

/// Refresh the queue listbox.
unsafe fn refresh_queue_list(hwnd: HWND) {
    let h_list = GetDlgItem(hwnd, IDC_LIST_QUEUE as i32);
    if h_list.is_null() {
        return;
    }
    SendMessageW(h_list, LB_RESETCONTENT, 0, 0);
    let queue = lock_or_recover(&SEQUENCE_QUEUE).clone();
    // (duration, leading delay) per item; a missing file shows and counts as 0:00.
    let mut timings: Vec<(i64, i64)> = Vec::with_capacity(queue.len());
    for (i, name) in queue.iter().enumerate() {
        let timing = storage::load_sequence(name)
            .map(|s| (s.duration_micros(), s.leading_delay_micros()))
            .unwrap_or((0, 0));
        timings.push(timing);
        let display = format!("{}. {} ({})", i + 1, name, sequence::format_duration(timing.0));
        let wname = wide(&display);
        SendMessageW(h_list, LB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
    }
    // Caption carries the pass total — what one Play Queue actually takes — and the saved
    // queue's name while the contents still match it.
    let title = match queue_label() {
        Some(label) => format!("Queue \"{}\"", label),
        None => "Queue".to_string(),
    };
    let caption = if queue.is_empty() {
        title
    } else {
        format!(
            "{} \u{2014} {} item{}, {}",
            title,
            queue.len(),
            if queue.len() == 1 { "" } else { "s" },
            sequence::format_duration(sequence::queue_pass_micros(&timings)),
        )
    };
    let h_caption = GetDlgItem(hwnd, IDC_STATIC_QUEUE_TOTAL as i32);
    if !h_caption.is_null() {
        SetWindowTextW(h_caption, wide(&caption).as_ptr());
    }
}
