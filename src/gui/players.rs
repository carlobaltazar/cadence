//! "Proximity — Players" window. Shows the players the detector has seen as a table
//! (Name/Lv/Class/School/Guild/HP/Map/Pos/Seen), sorted latest-first, across two lists you pick
//! with the radio buttons:
//!   * This session — everyone seen since Cadence launched.
//!   * All-time     — a persistent log (detected_players.json) that survives restarts.
//! Buttons act on whichever list is shown; the Clear button says which one. A separate Scan
//! toggle at the top runs a passive capture that logs players WITHOUT pressing the key.

use crate::win32_helpers::{create_control, dpi_for_window, register_and_create_dialog, scale, scaled_font, wide};
use crate::{config, mapcoord, proximity, xlsx};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::wingdi::{
    CreateFontW, DeleteObject, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_MODERN,
    FIXED_PITCH, FW_NORMAL, OUT_TT_PRECIS,
};
use winapi::um::winuser::*;

static PLAYERS_HWND: AtomicIsize = AtomicIsize::new(0);
// Previously rendered rows; None = repaint forced (a view/filter change must repaint even
// when the new render is empty, else the listbox keeps the other view's rows).
static LAST_RENDER: Mutex<Option<Vec<String>>> = Mutex::new(None);
// Player name for each rendered row, in display order — so a selection maps back to a name.
static ROW_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
// Which list is shown: false = this session, true = all-time (persistent).
static VIEW_PERMANENT: AtomicBool = AtomicBool::new(false);
static MONO_FONT: AtomicIsize = AtomicIsize::new(0);
// Case-insensitive filter (matches name or guild); empty = show all.
static SEARCH: Mutex<String> = Mutex::new(String::new());
// Attribute filters selected in the dropdowns; empty = "(Any)". Compared against the same display
// strings the table renders (class_name/school_name/map_name), so a match is exactly what you see.
static FILTER_CLASS: Mutex<String> = Mutex::new(String::new());
static FILTER_SCHOOL: Mutex<String> = Mutex::new(String::new());
static FILTER_MAP: Mutex<String> = Mutex::new(String::new());
static FILTER_GUILD: Mutex<String> = Mutex::new(String::new());
// Distinct map/guild names last pushed into their dropdowns, so we only repopulate on change.
static LAST_MAPS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LAST_GUILDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

// Fixed dropdown choices (index 0 is "(Any)"). Class ignores gender — the dropdown narrows by
// profession, matching class_name().0.
const CLASS_FILTER_NAMES: &[&str] =
    &["Brawler", "Swordsman", "Archer", "Shaman", "Extreme", "Gunner", "Assassin", "Tricker"];
const SCHOOL_FILTER_NAMES: &[&str] = &["SGate", "MPeak", "Phnix"];
const ANY_LABEL: &str = "(Any)";

// Column layout: (width, right_aligned). Shared by the header and every row so they line up.
const COLS: &[(usize, bool)] = &[
    (1, false),  // ignore marker
    (13, false), // name
    (3, true),   // level
    (9, false),  // class
    (1, false),  // gender
    (5, false),  // school
    (16, false), // map (name)
    (11, false), // guild
    (10, false), // nick
    (14, false), // tag: <badge> / GM account level
    (9, true),   // hp now/max
    (9, false),  // pos x,z
    (11, false), // seen
    (30, false), // why it tripped the alert (blank for everyone else)
];

fn class_name(c: u16) -> (&'static str, &'static str) {
    match c {
        0x0001 => ("Brawler", "M"),
        0x0040 => ("Brawler", "F"),
        0x0002 => ("Swordsman", "M"),
        0x0080 => ("Swordsman", "F"),
        0x0100 => ("Archer", "M"),
        0x0004 => ("Archer", "F"),
        0x0200 => ("Shaman", "M"),
        0x0008 => ("Shaman", "F"),
        0x0010 => ("Extreme", "M"),
        0x0020 => ("Extreme", "F"),
        0x0400 => ("Gunner", "M"),
        0x0800 => ("Gunner", "F"),
        0x1000 => ("Assassin", "M"),
        0x2000 => ("Assassin", "F"),
        0x4000 => ("Tricker", "M"),
        0x8000 => ("Tricker", "F"),
        _ => ("?", ""),
    }
}

fn school_name(s: u16) -> &'static str {
    match s {
        0 => "SGate",
        1 => "MPeak",
        2 => "Phnix",
        _ => "?",
    }
}

/// Map name for a DROP_PC map id (SNATIVEID.wMainID). Table decoded from RAN Portal's
/// Data\glogic\mapslist.mst (EMBYTECRYPT_MAPSLIST substitution cipher). Unknown ids fall back to
/// their number in the caller.
///
/// Ids 1..=30 were one slot too high until the mapslist was re-read against each record's own id
/// field: id 22 is PracticingYard (w_Total_suryun), not MarketPlace (w_tradezone1, id 21). The
/// reference DROP_PC capture confirms it — its map-22 positions only land on the in-game grid
/// with w_Total_suryun's axis origin (see `mapcoord`).
fn map_name(id: u16) -> Option<&'static str> {
    let n = match id {
        0 => "SG_Campus1F",
        1 => "SG_Campus",
        2 => "SacredGateHole",
        3 => "MP_Campus1F",
        4 => "MP_Campus",
        5 => "MysticPeakHole",
        6 => "PhoenixCampus1F",
        7 => "PhoenixCampus",
        8 => "PhoenixHole",
        9 => "LeonineCampus",
        10 => "LeonineCampus1F",
        11 => "LeonineCampus2F",
        12 => "LeonineCampus3F",
        13 => "LeonineCampusB1",
        14 => "LeonineCampusB2",
        15 => "TradingHole",
        16 => "SG HolePassage",
        17 => "WharfPassage",
        18 => "Hangout 1F_Ch1",
        19 => "Hangout 2F_Ch1",
        20 => "Hangout 3F_Ch1",
        21 => "MarketPlace",
        22 => "PracticingYard",
        23 => "Suryun",
        // 24..=30 follow the file's record order but their id bytes didn't decrypt; one id in
        // 24..=31 has no record, so a name here could be one slot out. Everything above and
        // below decrypted directly.
        24 => "ShibuyaII",
        25 => "Trading3Passage",
        26 => "Prison",
        27 => "Middle Hole",
        28 => "Root Hole",
        29 => "LeonineCampusB3",
        30 => "WeddingHall",
        32 => "PrisonTestZone",
        33 => "Labatory7",
        34 => "Shibuya",
        35 => "Head.B 30F",
        36 => "Head.B 50F",
        37 => "Head.B 90F",
        38 => "Head.B Left Wall",
        39 => "Head.B RightWall",
        40 | 41 => "Director Room",
        42 => "Head.B U-ground",
        43 => "Another W South",
        44 => "Another W centre",
        45 => "Another W North",
        46 => "Head.B 1F",
        47 => "Stadium",
        48 => "Evil Zone",
        51 => "Head.B 51F",
        52 => "Head.B 52F",
        61 => "ShibuyaIII",
        91 => "Cube Map",
        92 => "Swordsman Cube",
        93 => "Shaman Cube",
        94 => "Center of Cube",
        95 => "Archer Cube",
        96 => "Brawler Cube",
        100 => "StudyRoom1",
        101 => "StudyRoom2",
        102 => "Dormitory",
        103 => "StudyRoom3",
        104 => "HistoryCentre",
        105 => "Library",
        106 => "SocietyRoom",
        107 => "ScienceCentre",
        120 => "StudyRoom2",
        121 => "StudyRoom1",
        122 => "Library",
        123 => "ArtCentre",
        124 => "StudentCentre1",
        125 => "StudentCentre2",
        126 => "Dormitory",
        128 => "2nd Dormitory",
        130 => "StudyRoom2",
        131 => "StudyRoom1",
        132 => "PrisonTestZoneII",
        134 => "Dormitory",
        135 => "ScienceCentre",
        136 => "RenovationCentre",
        137 => "Laboratory",
        138 => "PracticeRoom",
        141 => "SuppliesRoom",
        142 => "Library",
        150 => "Hangout 1F_Ch2",
        151 => "Hangout 1F_Ch3",
        152 => "Hangout 2F_Ch2",
        153 => "Hangout 2F_Ch3",
        154 => "Hangout 3F_Ch2",
        155 => "Hangout 3F_Ch3",
        161 => "Carpark 1",
        162 => "Carpark 2",
        201 => "SG E-Room Front",
        202 => "MP E-Room Front",
        203 => "Pnx E-Room Front",
        204 => "Trd E-Room Front",
        211 => "SG E-Room",
        212 => "MP E-Room",
        213 => "Phoenix E-Room",
        214 => "Trading E-Room",
        222 => "Tyranny",
        227 => "PrisonII",
        250 => "Trading4Passage",
        _ => return None,
    };
    Some(n)
}

/// Map cell exactly as rendered in the table: decoded name, else the raw id, else "-".
/// Shared by the row renderer and the Map filter so a filter match is what you see.
fn map_display(p: &proximity::DetectedPlayer) -> String {
    match p.map {
        Some(id) => map_name(id).map(|s| s.to_string()).unwrap_or_else(|| id.to_string()),
        None => "-".into(),
    }
}

/// Position cell: the same coordinate the game prints on its minimap ("21/16"), so a sighting can
/// be walked to directly. DROP_PC carries raw 3D world units, which never matched what the player
/// sees on screen — `mapcoord` applies the client's own conversion. Maps whose axis origin isn't
/// known keep showing the world units rather than a plausible-looking wrong grid.
fn pos_display(p: &proximity::DetectedPlayer) -> String {
    if let Some(g) = mapcoord::grid_str(p.map, p.pos_x, p.pos_z) {
        return g;
    }
    match (p.pos_x, p.pos_z) {
        (Some(x), Some(z)) => format!("{},{}", x, z),
        _ => "-".into(),
    }
}

/// Guild cell as rendered in the table: the club name, or "-" when the player has none.
/// Shared by the row renderer and the Guild filter.
fn guild_display(p: &proximity::DetectedPlayer) -> String {
    if p.guild.is_empty() { "-".into() } else { p.guild.clone() }
}

/// Pad/truncate cells to the shared column widths and join with single spaces.
fn line(cells: &[String]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let (w, right) = COLS[i];
        let n = cell.chars().count();
        if n >= w {
            out.push_str(&cell.chars().take(w).collect::<String>());
        } else if right {
            for _ in 0..(w - n) {
                out.push(' ');
            }
            out.push_str(cell);
        } else {
            out.push_str(cell);
            for _ in 0..(w - n) {
                out.push(' ');
            }
        }
    }
    out
}

fn header_line() -> String {
    line(&[
        " ".into(), "Name".into(), "Lv".into(), "Class".into(), "G".into(), "Schl".into(),
        "Map".into(), "Guild".into(), "Nick".into(), "Tag".into(), "HP".into(), "Map x/y".into(),
        "Seen".into(), "Why it fired".into(),
    ])
}

fn row_line(p: &proximity::DetectedPlayer, ignored: bool) -> String {
    let (cls, gen) = p.class.map(class_name).unwrap_or(("-", ""));
    let sch = p.school.map(school_name).unwrap_or("-").to_string();
    let lv = p.level.map(|l| l.to_string()).unwrap_or_else(|| "-".into());
    let hp = match (p.hp_now, p.hp_max) {
        (Some(n), Some(m)) => format!("{}/{}", n, m),
        _ => "-".into(),
    };
    let map = map_display(p);
    let pos = pos_display(p);
    let guild = guild_display(p);
    let seen = if p.seen_str.is_empty() { "-".into() } else { p.seen_str.clone() };
    let nick = if p.nick.is_empty() { "-".into() } else { p.nick.clone() };
    line(&[
        if ignored { "*".into() } else { " ".into() },
        p.name.clone(), lv, cls.into(), gen.into(), sch, map, guild, nick, tag_display(p), hp, pos,
        seen, p.why.clone(),
    ])
}

/// Staff markers for the Tag column: the `<Badge>` the client draws above the name, plus the
/// account level when the character block reports a game-master account (EMUSERTYPE >= 19).
fn tag_display(p: &proximity::DetectedPlayer) -> String {
    let mut s = String::new();
    if !p.badge.is_empty() {
        s.push_str(&format!("<{}>", p.badge));
    }
    if let Some(l) = p.user_level.filter(|l| *l >= 19) {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("GM{}", l));
    }
    if s.is_empty() { "-".into() } else { s }
}

fn clear_label() -> &'static str {
    if VIEW_PERMANENT.load(Ordering::Acquire) {
        "Clear All-time"
    } else {
        "Clear Session"
    }
}

pub(crate) unsafe fn show_players_dialog(parent: HWND) {
    let existing = PLAYERS_HWND.load(Ordering::Acquire) as HWND;
    if !existing.is_null() && IsWindow(existing) != 0 {
        SetForegroundWindow(existing);
        return;
    }
    let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
    let mut pr: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut pr);
    let hwnd = register_and_create_dialog(
        "CadencePlayers",
        "Proximity \u{2014} Players",
        players_wnd_proc,
        WS_EX_TOOLWINDOW as u32,
        // Resizable now: WS_THICKFRAME + WS_MAXIMIZEBOX let the user widen the window (or maximize)
        // to reveal the right-hand columns; WM_SIZE reflows the table to fill.
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MAXIMIZEBOX | WS_VISIBLE,
        pr.left + 24,
        pr.top + 24,
        800,
        540,
        parent,
        hinstance,
    );
    PLAYERS_HWND.store(hwnd as isize, Ordering::Release);
}

/// Fetch the active list (session/all-time) and apply the text + attribute filters, sorted
/// latest-first. Shared by the on-screen table and the Excel export so both see the same rows.
fn collect_filtered() -> Vec<proximity::DetectedPlayer> {
    let permanent = VIEW_PERMANENT.load(Ordering::Acquire);
    let mut players = if permanent {
        proximity::permanent_players()
    } else {
        proximity::session_players()
    };
    let filter = SEARCH.lock().unwrap().to_lowercase();
    let fclass = FILTER_CLASS.lock().unwrap().clone();
    let fschool = FILTER_SCHOOL.lock().unwrap().clone();
    let fmap = FILTER_MAP.lock().unwrap().clone();
    let fguild = FILTER_GUILD.lock().unwrap().clone();
    players.retain(|p| {
        if !filter.is_empty()
            && !p.name.to_lowercase().contains(&filter)
            && !p.guild.to_lowercase().contains(&filter)
            && !p.badge.to_lowercase().contains(&filter)
            && !p.nick.to_lowercase().contains(&filter)
        {
            return false;
        }
        if !fclass.is_empty() && p.class.map(|c| class_name(c).0).unwrap_or("-") != fclass {
            return false;
        }
        if !fschool.is_empty() && p.school.map(school_name).unwrap_or("-") != fschool {
            return false;
        }
        if !fmap.is_empty() && map_display(p) != fmap {
            return false;
        }
        if !fguild.is_empty() && guild_display(p) != fguild {
            return false;
        }
        true
    });
    players.sort_by(|a, b| b.last_unix.cmp(&a.last_unix).then_with(|| a.name.cmp(&b.name)));
    players
}

/// Decide whether the listbox needs repainting and record the new render. `None` (set by
/// `force_refresh`) always repaints — critically, even when `lines` is empty, which is how a
/// view switch to an empty session list clears the rows of the previous view.
fn take_render(last: &mut Option<Vec<String>>, lines: &[String]) -> bool {
    if last.as_deref() == Some(lines) {
        return false;
    }
    *last = Some(lines.to_vec());
    true
}

unsafe fn refresh_list(hwnd: HWND) {
    let permanent = VIEW_PERMANENT.load(Ordering::Acquire);
    let full = if permanent {
        proximity::permanent_players()
    } else {
        proximity::session_players()
    };
    // Keep the Map/Guild dropdowns in sync with the values actually present (pre-filter).
    update_map_combo(hwnd, &full);
    update_guild_combo(hwnd, &full);

    let players = collect_filtered();
    let ignored = proximity::ignored_players();

    let mut lines: Vec<String> = Vec::with_capacity(players.len());
    let mut names: Vec<String> = Vec::with_capacity(players.len());
    for p in &players {
        lines.push(row_line(p, ignored.iter().any(|n| n == &p.name)));
        names.push(p.name.clone());
    }

    if !take_render(&mut LAST_RENDER.lock().unwrap(), &lines) {
        return;
    }

    let list = GetDlgItem(hwnd, IDC_LIST_PLAYERS as i32);
    // Preserve the selection by name (rows reorder as sightings update) and the scroll position.
    let prev_sel = {
        let sel = SendMessageW(list, LB_GETCURSEL, 0, 0);
        if sel >= 0 { ROW_NAMES.lock().unwrap().get(sel as usize).cloned() } else { None }
    };
    let top = SendMessageW(list, LB_GETTOPINDEX, 0, 0);

    SendMessageW(list, WM_SETREDRAW, 0, 0);
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    for l in &lines {
        SendMessageW(list, LB_ADDSTRING, 0, wide(l).as_ptr() as LPARAM);
    }
    if let Some(name) = prev_sel {
        if let Some(idx) = names.iter().position(|n| *n == name) {
            SendMessageW(list, LB_SETCURSEL, idx as WPARAM, 0);
        }
    }
    if top > 0 {
        SendMessageW(list, LB_SETTOPINDEX, top as WPARAM, 0);
    }
    SendMessageW(list, WM_SETREDRAW, 1, 0);
    InvalidateRect(list, std::ptr::null(), TRUE);

    *ROW_NAMES.lock().unwrap() = names;
}

unsafe fn force_refresh(hwnd: HWND) {
    *LAST_RENDER.lock().unwrap() = None;
    refresh_list(hwnd);
}

/// Currently-selected text of a combobox, with "(Any)" normalised to an empty string.
unsafe fn combo_sel_text(hwnd: HWND, id: u16) -> String {
    let h = GetDlgItem(hwnd, id as i32);
    let sel = SendMessageW(h, CB_GETCURSEL, 0, 0);
    if sel < 0 {
        return String::new();
    }
    let len = SendMessageW(h, CB_GETLBTEXTLEN, sel as WPARAM, 0);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    SendMessageW(h, CB_GETLBTEXT, sel as WPARAM, buf.as_mut_ptr() as LPARAM);
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    if s == ANY_LABEL { String::new() } else { s }
}

/// Rebuild the Map dropdown from the maps present in `players`, but only when that set changes
/// (so it doesn't fight the user mid-selection). Preserves the active choice, or falls back to
/// "(Any)" when the previously-selected map is no longer present.
unsafe fn update_map_combo(hwnd: HWND, players: &[proximity::DetectedPlayer]) {
    let mut maps: Vec<String> = players.iter().filter(|p| p.map.is_some()).map(map_display).collect();
    maps.sort();
    maps.dedup();
    {
        let mut last = LAST_MAPS.lock().unwrap();
        if *last == maps {
            return;
        }
        *last = maps.clone();
    }
    let cmb = GetDlgItem(hwnd, IDC_CMB_PLAYER_MAP as i32);
    if cmb.is_null() {
        return;
    }
    // Reconcile the stored selection against the new set before repopulating.
    let want = {
        let mut sel = FILTER_MAP.lock().unwrap();
        if !sel.is_empty() && !maps.iter().any(|m| m == &*sel) {
            sel.clear();
        }
        sel.clone()
    };
    SendMessageW(cmb, CB_RESETCONTENT, 0, 0);
    SendMessageW(cmb, CB_ADDSTRING, 0, wide(ANY_LABEL).as_ptr() as LPARAM);
    for m in &maps {
        SendMessageW(cmb, CB_ADDSTRING, 0, wide(m).as_ptr() as LPARAM);
    }
    let idx = if want.is_empty() {
        0
    } else {
        maps.iter().position(|m| *m == want).map(|p| p + 1).unwrap_or(0)
    };
    SendMessageW(cmb, CB_SETCURSEL, idx as WPARAM, 0);
}

/// Rebuild the Guild dropdown from the (non-empty) guilds present in `players`, only when that set
/// changes. Preserves the active choice, or falls back to "(Any)" if that guild is gone.
unsafe fn update_guild_combo(hwnd: HWND, players: &[proximity::DetectedPlayer]) {
    let mut guilds: Vec<String> =
        players.iter().filter(|p| !p.guild.is_empty()).map(|p| p.guild.clone()).collect();
    guilds.sort();
    guilds.dedup();
    {
        let mut last = LAST_GUILDS.lock().unwrap();
        if *last == guilds {
            return;
        }
        *last = guilds.clone();
    }
    let cmb = GetDlgItem(hwnd, IDC_CMB_PLAYER_GUILD as i32);
    if cmb.is_null() {
        return;
    }
    let want = {
        let mut sel = FILTER_GUILD.lock().unwrap();
        if !sel.is_empty() && !guilds.iter().any(|g| g == &*sel) {
            sel.clear();
        }
        sel.clone()
    };
    SendMessageW(cmb, CB_RESETCONTENT, 0, 0);
    SendMessageW(cmb, CB_ADDSTRING, 0, wide(ANY_LABEL).as_ptr() as LPARAM);
    for g in &guilds {
        SendMessageW(cmb, CB_ADDSTRING, 0, wide(g).as_ptr() as LPARAM);
    }
    let idx = if want.is_empty() {
        0
    } else {
        guilds.iter().position(|g| *g == want).map(|p| p + 1).unwrap_or(0)
    };
    SendMessageW(cmb, CB_SETCURSEL, idx as WPARAM, 0);
}

unsafe fn message_box(hwnd: HWND, text: &str, title: &str, flags: u32) {
    MessageBoxW(hwnd, wide(text).as_ptr(), wide(title).as_ptr(), flags);
}

/// Native "Save As" dialog seeded with `default_name`; returns the chosen path or None if cancelled.
unsafe fn save_file_dialog(hwnd: HWND, default_name: &str) -> Option<String> {
    use winapi::um::commdlg::{
        GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut file = wide(default_name);
    file.resize(1024, 0); // room for the full path the user picks
    let filter: Vec<u16> =
        "Excel Workbook (*.xlsx)\0*.xlsx\0All Files (*.*)\0*.*\0\0".encode_utf16().collect();
    let ext = wide("xlsx");
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if GetSaveFileNameW(&mut ofn) == 0 {
        return None; // cancelled or error
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(String::from_utf16_lossy(&file[..len]))
}

/// Export the currently-shown (filtered + sorted) players to an .xlsx workbook of the user's choice.
unsafe fn export_xlsx(hwnd: HWND) {
    let players = collect_filtered();
    if players.is_empty() {
        message_box(hwnd, "Nothing to export — the current view is empty.", "Export", MB_OK | MB_ICONINFORMATION);
        return;
    }
    let view = if VIEW_PERMANENT.load(Ordering::Acquire) { "alltime" } else { "session" };
    let default = format!("cadence_players_{}.xlsx", view);
    let path = match save_file_dialog(hwnd, &default) {
        Some(p) => p,
        None => return,
    };

    let headers = [
        "Name", "Level", "Class", "Gender", "School", "Map", "Guild", "Nick", "Tag",
        "HP Now", "HP Max", "Map X/Y", "World X", "World Z", "Sightings", "Seen", "Ignored",
        "Why It Fired",
    ];
    let ignored = proximity::ignored_players();
    let num_or_dash = |v: Option<u16>| match v {
        Some(n) => xlsx::Cell::Num(n as f64),
        None => xlsx::Cell::Str("-".into()),
    };
    let inum_or_dash = |v: Option<i32>| match v {
        Some(n) => xlsx::Cell::Num(n as f64),
        None => xlsx::Cell::Str("-".into()),
    };
    let mut rows: Vec<Vec<xlsx::Cell>> = Vec::with_capacity(players.len());
    for p in &players {
        let (cls, gen) = p.class.map(class_name).unwrap_or(("-", ""));
        let sch = p.school.map(school_name).unwrap_or("-");
        let seen = if p.seen_str.is_empty() { "-".to_string() } else { p.seen_str.clone() };
        let is_ignored = ignored.iter().any(|n| n == &p.name);
        rows.push(vec![
            xlsx::Cell::Str(p.name.clone()),
            num_or_dash(p.level),
            xlsx::Cell::Str(cls.to_string()),
            xlsx::Cell::Str(gen.to_string()),
            xlsx::Cell::Str(sch.to_string()),
            xlsx::Cell::Str(map_display(p)),
            xlsx::Cell::Str(guild_display(p)),
            xlsx::Cell::Str(if p.nick.is_empty() { "-".into() } else { p.nick.clone() }),
            xlsx::Cell::Str(tag_display(p)),
            num_or_dash(p.hp_now),
            num_or_dash(p.hp_max),
            xlsx::Cell::Str(mapcoord::grid_str(p.map, p.pos_x, p.pos_z).unwrap_or_else(|| "-".into())),
            inum_or_dash(p.pos_x),
            inum_or_dash(p.pos_z),
            xlsx::Cell::Num(p.count as f64),
            xlsx::Cell::Str(seen),
            xlsx::Cell::Str(if is_ignored { "yes".into() } else { String::new() }),
            xlsx::Cell::Str(p.why.clone()),
        ]);
    }

    match xlsx::write_sheet(std::path::Path::new(&path), &headers, &rows) {
        Ok(()) => message_box(
            hwnd,
            &format!("Exported {} players to:\n{}", players.len(), path),
            "Export complete",
            MB_OK | MB_ICONINFORMATION,
        ),
        Err(e) => message_box(hwnd, &format!("Export failed:\n{}", e), "Export", MB_OK | MB_ICONERROR),
    }
}

/// Reflow the stretchable controls (search box, header, list, button row) to the current client
/// size. Runs after creation and on every WM_SIZE. Coordinates are device pixels; the authored
/// 96-DPI design constants are scaled to the window DPI so the layout matches everything else.
unsafe fn layout(hwnd: HWND) {
    let dpi = dpi_for_window(hwnd);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (cw, ch) = (rc.right, rc.bottom);
    if cw <= 0 || ch <= 0 {
        return;
    }
    let m = scale(12, dpi);
    let gap = scale(6, dpi);
    let btn_h = scale(26, dpi);
    let btn_y = (ch - m - btn_h).max(0);
    let content_w = (cw - 2 * m).max(scale(120, dpi));

    // Search edit grows to follow the right edge.
    let search_x = scale(348, dpi);
    MoveWindow(
        GetDlgItem(hwnd, IDC_EDIT_PLAYER_SEARCH as i32),
        search_x, scale(64, dpi), (cw - m - search_x).max(scale(80, dpi)), scale(22, dpi), TRUE,
    );

    // Header + list span the width; the list fills the gap down to the button row.
    MoveWindow(GetDlgItem(hwnd, IDC_HDR_PLAYERS as i32), m, scale(118, dpi), content_w, scale(16, dpi), TRUE);
    let list = GetDlgItem(hwnd, IDC_LIST_PLAYERS as i32);
    let list_top = scale(136, dpi);
    let list_h = (btn_y - scale(8, dpi) - list_top).max(scale(60, dpi));
    MoveWindow(list, m, list_top, content_w, list_h, TRUE);
    SendMessageW(list, LB_SETHORIZONTALEXTENT, scale(860, dpi) as WPARAM, 0);

    // Bottom row: three actions left-aligned; Export + Close pinned to the right edge.
    let (tw, dw, cwid, exw, clw) =
        (scale(140, dpi), scale(80, dpi), scale(130, dpi), scale(90, dpi), scale(80, dpi));
    let mut x = m;
    MoveWindow(GetDlgItem(hwnd, IDC_BTN_PLAYER_TOGGLE as i32), x, btn_y, tw, btn_h, TRUE);
    x += tw + gap;
    MoveWindow(GetDlgItem(hwnd, IDC_BTN_PLAYER_DELETE as i32), x, btn_y, dw, btn_h, TRUE);
    x += dw + gap;
    MoveWindow(GetDlgItem(hwnd, IDC_BTN_PLAYER_CLEAR as i32), x, btn_y, cwid, btn_h, TRUE);
    let close_x = cw - m - clw;
    MoveWindow(GetDlgItem(hwnd, IDC_BTN_PLAYER_CLOSE as i32), close_x, btn_y, clw, btn_h, TRUE);
    MoveWindow(GetDlgItem(hwnd, IDC_BTN_PLAYER_EXPORT as i32), close_x - gap - exw, btn_y, exw, btn_h, TRUE);
}

unsafe fn read_edit_text(hwnd: HWND, id: u16) -> String {
    let h = GetDlgItem(hwnd, id as i32);
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn selected_name(hwnd: HWND) -> Option<String> {
    let list = GetDlgItem(hwnd, IDC_LIST_PLAYERS as i32);
    let sel = SendMessageW(list, LB_GETCURSEL, 0, 0);
    if sel < 0 {
        return None;
    }
    ROW_NAMES.lock().unwrap().get(sel as usize).cloned()
}

unsafe fn persist_ignore() {
    let mut c = config::cached_config();
    c.proximity_ignore = proximity::ignored_players();
    let _ = config::save_config(&c);
}

unsafe fn apply_scan(hwnd: HWND) {
    let chk = GetDlgItem(hwnd, IDC_CHK_PLAYER_SCAN as i32);
    let checked = SendMessageW(chk, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
    if checked {
        let cfg = config::cached_config();
        proximity::set_scan_only(true);
        proximity::start(
            cfg.proximity_vk,
            cfg.proximity_iface.clone(),
            cfg.proximity_server_ip.clone(),
            cfg.proximity_cooldown_ms,
        );
    } else {
        proximity::set_scan_only(false);
        proximity::stop();
    }
}

unsafe fn set_view(hwnd: HWND, permanent: bool) {
    VIEW_PERMANENT.store(permanent, Ordering::Release);
    SetWindowTextW(GetDlgItem(hwnd, IDC_BTN_PLAYER_CLEAR as i32), wide(clear_label()).as_ptr());
    force_refresh(hwnd);
}

/// Free per-window resources on destroy (runs however the window is closed — Close button, the
/// title-bar X, or the owner being destroyed). A passive scan is intentionally left RUNNING so it
/// keeps logging after the window closes; it stops only when you uncheck Scan, arm Det, or exit.
unsafe fn cleanup(hwnd: HWND) {
    KillTimer(hwnd, TIMER_PLAYERS);
    let font = MONO_FONT.swap(0, Ordering::AcqRel) as HFONT;
    if !font.is_null() {
        DeleteObject(font as _);
    }
    PLAYERS_HWND.store(0, Ordering::Release);
}

unsafe extern "system" fn players_wnd_proc(hwnd: HWND, msg: UINT, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = winapi::um::libloaderapi::GetModuleHandleW(std::ptr::null());
            let dpi = dpi_for_window(hwnd);
            let font = scaled_font(dpi);
            let mono = CreateFontW(
                -(9 * dpi as i32) / 72,
                0, 0, 0, FW_NORMAL as i32, 0, 0, 0,
                DEFAULT_CHARSET, OUT_TT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                FIXED_PITCH | FF_MODERN,
                wide("Consolas").as_ptr(),
            );
            MONO_FONT.store(mono as isize, Ordering::Release);

            // --- Scan section (top, separated) ---
            create_control(
                hwnd, hinstance, font, "STATIC",
                "Passive scan \u{2014} logs everyone in view WITHOUT pressing the key (won\u{2019}t trip Det):",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 8, 620, 16, 0,
            );
            let chk_scan = create_control(
                hwnd, hinstance, font, "BUTTON", "Scan on",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32, 0,
                12, 28, 90, 22, IDC_CHK_PLAYER_SCAN,
            );
            if proximity::is_scan_only() {
                SendMessageW(chk_scan, BM_SETCHECK, BST_CHECKED as WPARAM, 0);
            }
            create_control(
                hwnd, hinstance, font, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_ETCHEDHORZ, 0,
                12, 56, 620, 2, 0,
            );

            // --- View selector (radios: current view always visible) ---
            create_control(
                hwnd, hinstance, font, "STATIC", "Show:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 66, 40, 20, 0,
            );
            let rad_sess = create_control(
                hwnd, hinstance, font, "BUTTON", "This session",
                WS_CHILD | WS_VISIBLE | WS_GROUP | WS_TABSTOP | BS_AUTORADIOBUTTON as u32, 0,
                54, 64, 110, 22, IDC_RADIO_SESSION,
            );
            let rad_all = create_control(
                hwnd, hinstance, font, "BUTTON", "All-time",
                WS_CHILD | WS_VISIBLE | BS_AUTORADIOBUTTON as u32, 0,
                168, 64, 90, 22, IDC_RADIO_ALLTIME,
            );
            let permanent = VIEW_PERMANENT.load(Ordering::Acquire);
            SendMessageW(if permanent { rad_all } else { rad_sess }, BM_SETCHECK, BST_CHECKED as WPARAM, 0);

            // Search / filter box (matches name or guild). Reset the filter to match the empty box.
            create_control(
                hwnd, hinstance, font, "STATIC", "Search:",
                WS_CHILD | WS_VISIBLE | SS_RIGHT, 0,
                288, 66, 56, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "EDIT", "",
                WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE as u32,
                348, 64, 284, 22, IDC_EDIT_PLAYER_SEARCH,
            );
            *SEARCH.lock().unwrap() = String::new();

            // --- Attribute filters (Class / School / Map / Guild), AND-combined with the search ---
            *FILTER_CLASS.lock().unwrap() = String::new();
            *FILTER_SCHOOL.lock().unwrap() = String::new();
            *FILTER_MAP.lock().unwrap() = String::new();
            *FILTER_GUILD.lock().unwrap() = String::new();
            LAST_MAPS.lock().unwrap().clear();
            LAST_GUILDS.lock().unwrap().clear();
            let combo_style =
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32;
            // Positions fit inside the enforced minimum width so nothing clips when shrunk.
            create_control(
                hwnd, hinstance, font, "STATIC", "Class:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 12, 92, 36, 20, 0,
            );
            let cmb_class = create_control(
                hwnd, hinstance, font, "COMBOBOX", "", combo_style, 0,
                50, 89, 90, 200, IDC_CMB_PLAYER_CLASS,
            );
            create_control(
                hwnd, hinstance, font, "STATIC", "School:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 148, 92, 46, 20, 0,
            );
            let cmb_school = create_control(
                hwnd, hinstance, font, "COMBOBOX", "", combo_style, 0,
                196, 89, 66, 200, IDC_CMB_PLAYER_SCHOOL,
            );
            create_control(
                hwnd, hinstance, font, "STATIC", "Map:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 270, 92, 30, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "", combo_style, 0,
                302, 89, 130, 240, IDC_CMB_PLAYER_MAP,
            );
            create_control(
                hwnd, hinstance, font, "STATIC", "Guild:",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0, 440, 92, 40, 20, 0,
            );
            create_control(
                hwnd, hinstance, font, "COMBOBOX", "", combo_style, 0,
                482, 89, 110, 240, IDC_CMB_PLAYER_GUILD,
            );
            // Fixed choices (index 0 = "(Any)"). Map/Guild combos are filled from live data on refresh.
            for (h, names) in [(cmb_class, CLASS_FILTER_NAMES), (cmb_school, SCHOOL_FILTER_NAMES)] {
                SendMessageW(h, CB_ADDSTRING, 0, wide(ANY_LABEL).as_ptr() as LPARAM);
                for n in names {
                    SendMessageW(h, CB_ADDSTRING, 0, wide(n).as_ptr() as LPARAM);
                }
                SendMessageW(h, CB_SETCURSEL, 0, 0);
            }
            for id in [IDC_CMB_PLAYER_MAP, IDC_CMB_PLAYER_GUILD] {
                let cmb = GetDlgItem(hwnd, id as i32);
                SendMessageW(cmb, CB_ADDSTRING, 0, wide(ANY_LABEL).as_ptr() as LPARAM);
                SendMessageW(cmb, CB_SETCURSEL, 0, 0);
            }

            // --- Table: monospace header + list ---
            let hdr = create_control(
                hwnd, hinstance, mono, "STATIC", "",
                WS_CHILD | WS_VISIBLE | SS_LEFT, 0,
                12, 118, 620, 16, IDC_HDR_PLAYERS,
            );
            SendMessageW(hdr, WM_SETTEXT, 0, wide(&header_line()).as_ptr() as LPARAM);
            let list = create_control(
                hwnd, hinstance, mono, "LISTBOX", "",
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | LBS_NOTIFY as u32,
                WS_EX_CLIENTEDGE as u32,
                12, 136, 620, 260, IDC_LIST_PLAYERS,
            );
            SendMessageW(list, WM_SETFONT, mono as WPARAM, 1);
            SendMessageW(list, LB_SETHORIZONTALEXTENT, 860, 0);

            // --- Actions ---
            create_control(
                hwnd, hinstance, font, "BUTTON", "Ignore / Unignore",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                12, 416, 140, 26, IDC_BTN_PLAYER_TOGGLE,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Delete",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                158, 416, 80, 26, IDC_BTN_PLAYER_DELETE,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", clear_label(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                244, 416, 130, 26, IDC_BTN_PLAYER_CLEAR,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Export\u{2026}",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                462, 416, 90, 26, IDC_BTN_PLAYER_EXPORT,
            );
            create_control(
                hwnd, hinstance, font, "BUTTON", "Close",
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32, 0,
                552, 416, 80, 26, IDC_BTN_PLAYER_CLOSE,
            );

            *LAST_RENDER.lock().unwrap() = None;
            refresh_list(hwnd);
            layout(hwnd); // size the stretchable controls to the initial client area
            SetTimer(hwnd, TIMER_PLAYERS, 1500, None);
            0
        }
        WM_TIMER => {
            if w_param == TIMER_PLAYERS {
                refresh_list(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = LOWORD(w_param as u32);
            let code = HIWORD(w_param as u32);
            if id == IDC_EDIT_PLAYER_SEARCH && code == EN_CHANGE {
                *SEARCH.lock().unwrap() = read_edit_text(hwnd, IDC_EDIT_PLAYER_SEARCH);
                force_refresh(hwnd);
                return 0;
            }
            if code == CBN_SELCHANGE
                && matches!(
                    id,
                    IDC_CMB_PLAYER_CLASS
                        | IDC_CMB_PLAYER_SCHOOL
                        | IDC_CMB_PLAYER_MAP
                        | IDC_CMB_PLAYER_GUILD
                )
            {
                let text = combo_sel_text(hwnd, id);
                match id {
                    IDC_CMB_PLAYER_CLASS => *FILTER_CLASS.lock().unwrap() = text,
                    IDC_CMB_PLAYER_SCHOOL => *FILTER_SCHOOL.lock().unwrap() = text,
                    IDC_CMB_PLAYER_MAP => *FILTER_MAP.lock().unwrap() = text,
                    _ => *FILTER_GUILD.lock().unwrap() = text,
                }
                force_refresh(hwnd);
                return 0;
            }
            if id == IDC_BTN_PLAYER_TOGGLE {
                if let Some(name) = selected_name(hwnd) {
                    if !name.is_empty() {
                        proximity::toggle_ignore(&name);
                        persist_ignore();
                        force_refresh(hwnd);
                    }
                }
            } else if id == IDC_BTN_PLAYER_DELETE {
                if let Some(name) = selected_name(hwnd) {
                    proximity::delete_player(VIEW_PERMANENT.load(Ordering::Acquire), &name);
                    force_refresh(hwnd);
                }
            } else if id == IDC_BTN_PLAYER_CLEAR {
                if VIEW_PERMANENT.load(Ordering::Acquire) {
                    proximity::clear_permanent();
                } else {
                    proximity::clear_session();
                }
                force_refresh(hwnd);
            } else if id == IDC_RADIO_SESSION {
                set_view(hwnd, false);
            } else if id == IDC_RADIO_ALLTIME {
                set_view(hwnd, true);
            } else if id == IDC_CHK_PLAYER_SCAN {
                apply_scan(hwnd);
            } else if id == IDC_BTN_PLAYER_EXPORT {
                export_xlsx(hwnd);
            } else if id == IDC_BTN_PLAYER_CLOSE {
                DestroyWindow(hwnd);
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
            mmi.ptMinTrackSize.x = scale(620, dpi);
            mmi.ptMinTrackSize.y = scale(380, dpi);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            cleanup(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

#[cfg(test)]
mod tests {
    use super::take_render;

    #[test]
    fn forced_render_repaints_even_when_empty() {
        let mut last = None; // the force_refresh state, e.g. right after a view switch
        assert!(take_render(&mut last, &[]), "empty render must still clear the listbox");
        assert_eq!(last, Some(Vec::new()));
        assert!(!take_render(&mut last, &[]), "unchanged render skips the repaint");
        let rows = vec!["row".to_string()];
        assert!(take_render(&mut last, &rows));
        assert!(!take_render(&mut last, &rows));
    }
}
