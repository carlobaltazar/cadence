use crate::sequence::RemoteBinding;
use crate::win32_helpers::lock_or_recover;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

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
    /// Pet Guard: when the MP/SP monitor reads low (impossible with the regen pet out),
    /// press the pet toggle to call it back, and flag "pet might be hungry" if it stays low.
    #[serde(default)]
    pub pet_guard_enabled: bool,
    /// Seconds MP/SP must stay low (despite the pet call) before reporting hungry.
    #[serde(default = "default_pet_guard_hungry_secs")]
    pub pet_guard_hungry_secs: u32,
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
    #[serde(default)]
    pub proximity_enabled: bool,
    #[serde(default = "default_proximity_vk")]
    pub proximity_vk: u16,
    #[serde(default)]
    pub proximity_iface: String,
    #[serde(default = "default_proximity_server_ip")]
    pub proximity_server_ip: String,
    #[serde(default = "default_proximity_cooldown")]
    pub proximity_cooldown_ms: u64,
    #[serde(default)]
    pub proximity_ignore: Vec<String>,
    /// Reaction sequence to play on detection. Empty = press the proximity key instead.
    #[serde(default)]
    pub proximity_sequence: String,
    /// Trigger: false = react to any player, true = react only to GM/watch-list matches.
    #[serde(default)]
    pub proximity_watch_only: bool,
    /// Name/badge/nick/guild patterns that count as a GM in watch mode.
    #[serde(default = "default_proximity_watch")]
    pub proximity_watch: Vec<String>,
    /// Also trigger on a staff account level read from the character block, whatever the name.
    #[serde(default = "default_true")]
    pub proximity_watch_gm_flag: bool,
    /// Check GitHub for a newer release a few seconds after launch.
    #[serde(default = "default_true")]
    pub update_check_on_start: bool,
    /// A version the user chose to skip; it is found but never prompted for again.
    #[serde(default)]
    pub update_skip_version: String,
    /// Minutes between periodic update polls while running, so a 24/7 VM notices new
    /// releases without a relaunch. 0 = check at launch only.
    #[serde(default = "default_update_interval_mins")]
    pub update_check_interval_mins: u32,
    /// Seconds before the update prompt auto-answers Yes, so unattended VMs update
    /// themselves. 0 = wait for a human.
    #[serde(default = "default_update_auto_confirm_secs")]
    pub update_auto_confirm_secs: u32,
    /// After an update restart, resume the sequence/queue that was playing.
    #[serde(default = "default_true")]
    pub update_auto_resume: bool,
    /// Push heartbeats/events to the fleet dashboard. Defaults on so an exe rollout
    /// lights up every machine with no per-machine config edits; opt out here.
    #[serde(default = "default_true")]
    pub report_enabled: bool,
    #[serde(default = "default_report_url")]
    pub report_url: String,
    /// Shared fleet token; must match the dashboard server's AGENT_TOKEN.
    #[serde(default = "default_report_token")]
    pub report_token: String,
    /// Optional human label shown next to the machine name on the dashboard
    /// (e.g. the character name). Empty = machine name only.
    #[serde(default)]
    pub report_label: String,
}

fn default_remote_port() -> u16 { 9847 }
fn default_pet_cycle_interval() -> u64 { 120 }
fn default_pet_guard_hungry_secs() -> u32 { 5 }
fn default_burst_rate_hz() -> u32 { 100 }
fn default_burst_vk() -> u16 { 0x14 } // Caps Lock
fn default_proximity_vk() -> u16 { 0x45 } // 'E'
// Blank = auto-discover the busiest non-web game endpoint (works when the server remaps ports,
// e.g. RAN Portal on 51.79.253.42:8104). Set an explicit IP in Settings only to override.
fn default_proximity_server_ip() -> String { String::new() }
fn default_proximity_cooldown() -> u64 { 500 }
fn default_true() -> bool { true }
fn default_update_interval_mins() -> u32 { 30 }
fn default_update_auto_confirm_secs() -> u32 { 60 }
fn default_report_url() -> String { "https://147-182-215-152.sslip.io/api/report".to_string() }
fn default_report_token() -> String { "ccdea6d580af677289951e42c88f7b34763eeb0f1ced6534".to_string() }
fn default_proximity_watch() -> Vec<String> {
    crate::proximity::DEFAULT_WATCH.iter().map(|s| s.to_string()).collect()
}

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
            pet_guard_enabled: false,
            pet_guard_hungry_secs: default_pet_guard_hungry_secs(),
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
            proximity_enabled: false,
            proximity_vk: default_proximity_vk(),
            proximity_iface: String::new(),
            proximity_server_ip: default_proximity_server_ip(),
            proximity_cooldown_ms: default_proximity_cooldown(),
            proximity_ignore: Vec::new(),
            proximity_sequence: String::new(),
            proximity_watch_only: false,
            proximity_watch: default_proximity_watch(),
            proximity_watch_gm_flag: true,
            update_check_on_start: true,
            update_skip_version: String::new(),
            update_check_interval_mins: default_update_interval_mins(),
            update_auto_confirm_secs: default_update_auto_confirm_secs(),
            update_auto_resume: true,
            report_enabled: true,
            report_url: default_report_url(),
            report_token: default_report_token(),
            report_label: String::new(),
        }
    }
}

fn config_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("ranify2");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[Cadence] Failed to create config dir: {}", e);
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

// Process-global config cache. Hotkey handlers in the message loop (burst
// toggle, remote send) read config on every keypress; without this each read
// was a file open + full JSON parse. Populated lazily on first `cached_config`
// call and kept fresh by `save_config`, which is the sole write path.
static CONFIG_CACHE: Mutex<Option<AppConfig>> = Mutex::new(None);

/// Config snapshot backed by the in-memory cache, avoiding a disk read + parse
/// on hot paths. Returns identical values to `load_config`; use this anywhere a
/// fresh-from-disk read isn't required.
pub fn cached_config() -> AppConfig {
    lock_or_recover(&CONFIG_CACHE)
        .get_or_insert_with(load_config)
        .clone()
}

pub fn save_config(config: &AppConfig) -> std::io::Result<()> {
    let path = config_path();
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, json)?;
    // Keep the cache in sync so hotkey handlers see saved changes immediately.
    *lock_or_recover(&CONFIG_CACHE) = Some(config.clone());
    Ok(())
}
