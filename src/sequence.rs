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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RemoteBinding {
    pub modifiers: u32,
    pub vk_code: u16,
    pub sequence_name: String,
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

    pub fn set_hotkey(&mut self, vk_code: u16) {
        self.hotkey = Some(HotkeyBinding { modifiers: 0, vk_code });
    }

    pub fn clear_hotkey(&mut self) {
        self.hotkey = None;
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}
