use crate::sequence::RemoteBinding;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_RECORD_VK: u16 = 0x77; // F8
pub const DEFAULT_STOP_VK: u16 = 0x7A;   // F11

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub record_vk: u16,
    #[serde(alias = "play_vk")]
    pub stop_vk: u16,
    pub loop_playback: bool,
    pub always_on_top: bool,
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    #[serde(default)]
    pub remote_password: String,
    #[serde(default)]
    pub remote_auto_listen: bool,
    #[serde(default)]
    pub remote_hosts: Vec<String>,
    #[serde(default)]
    pub remote_bindings: Vec<RemoteBinding>,
    #[serde(default)]
    pub shuffle_queue: bool,
    #[serde(default)]
    pub queue_vk: Option<u16>,
    #[serde(default)]
    pub default_sequence: Option<String>,
    #[serde(default)]
    pub pet_cycle_enabled: bool,
    #[serde(default = "default_pet_cycle_interval")]
    pub pet_cycle_interval_secs: u64,
    #[serde(default)]
    pub hp_monitor_enabled: bool,
    #[serde(default)]
    pub hp_monitor_x: i32,
    #[serde(default)]
    pub hp_monitor_y: i32,
    #[serde(default)]
    pub hp_monitor_color: u32,
    #[serde(default)]
    pub hp_monitor_window_class: String,
    #[serde(default)]
    pub hp_monitor_window_title: String,
    #[serde(default)]
    pub mp_monitor_enabled: bool,
    #[serde(default)]
    pub mp_monitor_x: i32,
    #[serde(default)]
    pub mp_monitor_y: i32,
    #[serde(default)]
    pub mp_monitor_color: u32,
    #[serde(default)]
    pub sp_monitor_enabled: bool,
    #[serde(default)]
    pub sp_monitor_x: i32,
    #[serde(default)]
    pub sp_monitor_y: i32,
    #[serde(default)]
    pub sp_monitor_color: u32,
    #[serde(default = "default_burst_rate_hz")]
    pub burst_rate_hz: u32,
    #[serde(default = "default_burst_vk")]
    pub burst_vk: u16,
}

fn default_remote_port() -> u16 { 9847 }
fn default_pet_cycle_interval() -> u64 { 120 }
fn default_burst_rate_hz() -> u32 { 100 }
fn default_burst_vk() -> u16 { 0x14 } // Caps Lock

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            record_vk: DEFAULT_RECORD_VK,
            stop_vk: DEFAULT_STOP_VK,
            loop_playback: false,
            always_on_top: true,
            remote_port: default_remote_port(),
            remote_password: String::new(),
            remote_auto_listen: false,
            remote_hosts: Vec::new(),
            remote_bindings: Vec::new(),
            shuffle_queue: false,
            queue_vk: None,
            default_sequence: None,
            pet_cycle_enabled: false,
            pet_cycle_interval_secs: default_pet_cycle_interval(),
            hp_monitor_enabled: false,
            hp_monitor_x: 0,
            hp_monitor_y: 0,
            hp_monitor_color: 0,
            hp_monitor_window_class: String::new(),
            hp_monitor_window_title: String::new(),
            mp_monitor_enabled: false,
            mp_monitor_x: 0,
            mp_monitor_y: 0,
            mp_monitor_color: 0,
            sp_monitor_enabled: false,
            sp_monitor_x: 0,
            sp_monitor_y: 0,
            sp_monitor_color: 0,
            burst_rate_hz: default_burst_rate_hz(),
            burst_vk: default_burst_vk(),
        }
    }
}

fn config_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("ranify2");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[Ranify2] Failed to create config dir: {}", e);
    }
    dir.join("config.json")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    if let Ok(json) = fs::read_to_string(&path) {
        serde_json::from_str(&json).unwrap_or_default()
    } else {
        AppConfig::default()
    }
}

pub fn save_config(config: &AppConfig) -> std::io::Result<()> {
    let path = config_path();
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, json)
}
