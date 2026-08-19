use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InputEvent {
    /// Microseconds since the previous event
    pub delay_micros: i64,
    pub event_type: InputEventType,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum InputEventType {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    MouseWheel { delta: i32 },
    KeyPress { vk_code: u16, scan_code: u16, pressed: bool, extended: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HotkeyBinding {
    pub modifiers: u32,
    pub vk_code: u16,
}

/// What a remote hotkey fires on the hosts. Legacy bindings (no field) are sequences.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum BindingTarget {
    #[default]
    Sequence,
    /// A saved queue (see `SavedQueue`), expanded by the SENDER into sequence names.
    Queue,
    /// A sequence group, expanded by the SENDER in group order.
    Group,
}

impl BindingTarget {
    pub const ALL: [BindingTarget; 3] = [BindingTarget::Sequence, BindingTarget::Queue, BindingTarget::Group];

    pub fn label(self) -> &'static str {
        match self {
            BindingTarget::Sequence => "Sequence",
            BindingTarget::Queue => "Saved queue",
            BindingTarget::Group => "Group",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RemoteBinding {
    pub modifiers: u32,
    pub vk_code: u16,
    /// Name of the target (JSON key kept from when only sequences could be bound).
    pub sequence_name: String,
    #[serde(default)]
    pub target: BindingTarget,
}

impl RemoteBinding {
    /// The wire command this binding sends. Saved queues / groups are resolved BY NAME on the
    /// host (its own `up` wins); the sender's expansion — `resolve`, injected so the mapping is
    /// testable without touching disk — rides along as a fallback for hosts that lack the name,
    /// and may be empty. Always sends something, so a missing local name is never silent.
    pub fn command(&self, resolve: impl Fn(BindingTarget, &str) -> Vec<String>) -> String {
        let verb = match self.target {
            BindingTarget::Sequence => return format!("PLAY {}", self.sequence_name),
            BindingTarget::Queue => "PLAY_SAVED",
            BindingTarget::Group => "PLAY_GROUP",
        };
        let names = resolve(self.target, &self.sequence_name);
        if names.is_empty() {
            format!("{} {}", verb, self.sequence_name)
        } else {
            format!("{} {} {}", verb, self.sequence_name, names.join(" "))
        }
    }
}

/// A named, ordered play list of sequence names. Unlike a group it may repeat a sequence and
/// one sequence can sit in any number of saved queues. Identity is the sanitized file stem,
/// exactly like sequences (`storage::sanitize_filename`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedQueue {
    pub name: String,
    pub items: Vec<String>,
    #[serde(default)]
    pub created_at: String,
}

impl SavedQueue {
    pub fn new(name: String, items: Vec<String>) -> Self {
        SavedQueue { name, items, created_at: chrono_now() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sequence {
    pub name: String,
    pub hotkey: Option<HotkeyBinding>,
    pub events: Vec<InputEvent>,
    pub created_at: String,
    pub total_duration_micros: i64,
    #[serde(default)]
    pub group: Option<String>,
    /// Play/display position inside its group (ties broken by name). Legacy files default to 0,
    /// which keeps them alphabetical.
    #[serde(default)]
    pub group_order: u32,
}

impl Sequence {
    pub fn new(name: String, events: Vec<InputEvent>) -> Self {
        let total_duration_micros: i64 = events.iter().map(|e| e.delay_micros).sum();
        let created_at = chrono_now();
        Sequence {
            name,
            hotkey: None,
            events,
            created_at,
            total_duration_micros,
            group: None,
            group_order: 0,
        }
    }

    /// A copy of this recording under a new identity. Hotkeys are not copied — a key maps
    /// to exactly one sequence.
    pub fn copy_as(&self, name: String, group: Option<String>, group_order: u32) -> Sequence {
        Sequence {
            name,
            hotkey: None,
            events: self.events.clone(),
            created_at: chrono_now(),
            total_duration_micros: self.total_duration_micros,
            group,
            group_order,
        }
    }

    /// Wall time of one playback, recomputed from the events (the stored
    /// `total_duration_micros` is only what was written at save time).
    pub fn duration_micros(&self) -> i64 {
        self.events.iter().map(|e| e.delay_micros).sum()
    }

    /// Delay before the first event — the part the player skips for every queue item after
    /// the first (see `player::run_playback_opts`).
    pub fn leading_delay_micros(&self) -> i64 {
        self.events.first().map_or(0, |e| e.delay_micros)
    }

    pub fn set_hotkey(&mut self, vk_code: u16) {
        self.hotkey = Some(HotkeyBinding { modifiers: 0, vk_code });
    }

    pub fn clear_hotkey(&mut self) {
        self.hotkey = None;
    }
}

/// Wall time of one pass over a queue, given each item's `(duration, leading_delay)` in
/// micros. Mirrors the player: the first item plays in full, later items start immediately
/// (their leading delay is zeroed).
pub fn queue_pass_micros(items: &[(i64, i64)]) -> i64 {
    items
        .iter()
        .enumerate()
        .map(|(i, &(duration, leading))| if i == 0 { duration } else { duration - leading })
        .sum()
}

/// The low-level keyboard hook reports Right Shift (0xA1, scan 0x36) and NumLock (0x90, scan
/// 0x45) with the extended flag set although neither is an extended key. Replaying that flag
/// injects `E0 36` / `E0 45`, which Windows treats as the "fake shift" prefix / part of Pause and
/// discards — so Shift+`-` came back as `-`. Drop the flag for those two.
pub fn fix_extended(vk_code: u16, extended: bool) -> bool {
    match vk_code {
        0xA1 | 0x90 => false,
        _ => extended,
    }
}

/// `m:ss`, or `h:mm:ss` from one hour up, rounded to the nearest second.
pub fn format_duration(micros: i64) -> String {
    let secs = (micros.max(0) + 500_000) / 1_000_000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_by_magnitude() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(-5), "0:00");
        assert_eq!(format_duration(59_400_000), "0:59");
        assert_eq!(format_duration(59_600_000), "1:00");
        assert_eq!(format_duration(83_000_000), "1:23");
        assert_eq!(format_duration(3_600_000_000), "1:00:00");
        assert_eq!(format_duration(3_723_000_000), "1:02:03");
    }

    #[test]
    fn extended_flag_dropped_for_misreported_keys() {
        assert!(!fix_extended(0xA1, true)); // Right Shift
        assert!(!fix_extended(0x90, true)); // NumLock
        assert!(fix_extended(0x27, true));  // Right arrow really is extended
        assert!(!fix_extended(0x41, false)); // 'A'
    }

    #[test]
    fn queue_pass_skips_later_leading_delays() {
        assert_eq!(queue_pass_micros(&[]), 0);
        assert_eq!(queue_pass_micros(&[(10, 3)]), 10);
        assert_eq!(queue_pass_micros(&[(10, 3), (20, 4), (30, 5)]), 10 + 16 + 25);
    }

    #[test]
    fn remote_binding_command_per_target() {
        let resolve = |t: BindingTarget, name: &str| match (t, name) {
            (BindingTarget::Queue, "night") => vec!["a".to_string(), "b".to_string(), "a".to_string()],
            (BindingTarget::Group, "buffs") => vec!["x".to_string()],
            _ => Vec::new(),
        };
        let mk = |target, name: &str| RemoteBinding {
            modifiers: 2, vk_code: 0x51, sequence_name: name.to_string(), target,
        };
        assert_eq!(mk(BindingTarget::Sequence, "farm").command(resolve), "PLAY farm");
        assert_eq!(mk(BindingTarget::Queue, "night").command(resolve), "PLAY_SAVED night a b a");
        assert_eq!(mk(BindingTarget::Group, "buffs").command(resolve), "PLAY_GROUP buffs x");
        // Sender has no such queue/group: still sent, by name only — the host resolves it.
        assert_eq!(mk(BindingTarget::Queue, "up").command(resolve), "PLAY_SAVED up");
        assert_eq!(mk(BindingTarget::Group, "empty").command(resolve), "PLAY_GROUP empty");
        // Legacy config without a target field still parses as a sequence binding.
        let legacy: RemoteBinding =
            serde_json::from_str(r#"{"modifiers":4,"vk_code":82,"sequence_name":"buff_ht"}"#).unwrap();
        assert_eq!(legacy.target, BindingTarget::Sequence);
    }
}
