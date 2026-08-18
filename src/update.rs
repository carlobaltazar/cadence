//! In-app updater. Cadence is distributed as a bare `cadence.exe` copied onto each machine, so
//! every release used to mean re-downloading it everywhere. This checks the public GitHub
//! releases API on launch and, with the user's consent, replaces the running exe in place.
//!
//! HTTPS goes through WinHTTP (schannel + the system cert store) rather than a Rust TLS crate —
//! `winapi` is already a dependency, so this adds no third-party code to a build that the game's
//! anti-cheat watches. Same spirit as `proximity.rs` binding wpcap directly.

use crate::win32_helpers::wide;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use winapi::ctypes::c_int;
use winapi::shared::minwindef::{DWORD, LPVOID};
use winapi::um::winhttp::*;
use winapi::um::winuser::PostMessageW;

/// Repo the updater tracks. Public, so every request below is unauthenticated.
const REPO: &str = "carlobaltazar/cadence";
/// Release asset to install. `.github/workflows/release.yml` must keep publishing this exact
/// name — renaming the asset would silently break every deployed client's update check.
const ASSET: &str = "cadence.exe";
/// Downloads must come from here. Guards against a redirected/spoofed asset URL in the API reply.
const ASSET_URL_PREFIX: &str = "https://github.com/carlobaltazar/cadence/releases/download/";
pub const RELEASES_PAGE: &str = "https://github.com/carlobaltazar/cadence/releases/latest";

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Sanity bounds for the downloaded exe: a release build is ~1MB, an error page is a few KB.
const MIN_EXE_BYTES: usize = 300 * 1024;
const MAX_EXE_BYTES: usize = 64 * 1024 * 1024;

/// A release newer than this build.
#[derive(Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub size: u64,
}

// Filled by a check, read by the UI thread when it handles the posted message.
static PENDING: Mutex<Option<UpdateInfo>> = Mutex::new(None);

// The newest release the background poll has seen, for the passive "vX available" notice in
// the toolbar title and on the Settings Update button. Nothing acts on it by itself.
static AVAILABLE: Mutex<Option<UpdateInfo>> = Mutex::new(None);

// True while a manual install is in flight, so the periodic poll stays quiet and a second
// press of Update can't start a second download. Never cleared on a successful apply — that
// process is exiting.
static PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_prompt_active(active: bool) {
    PROMPT_ACTIVE.store(active, Ordering::Release);
}

pub fn prompt_active() -> bool {
    PROMPT_ACTIVE.load(Ordering::Acquire)
}

/// The version of this build, for display in the UI.
pub fn current_version() -> &'static str {
    VERSION
}

/// Take the update staged for the UI thread, if any.
pub fn take_pending() -> Option<UpdateInfo> {
    PENDING.lock().unwrap().take()
}

/// Stage an update for the UI thread (background notice or manual install).
pub fn set_pending(info: UpdateInfo) {
    *PENDING.lock().unwrap() = Some(info);
}

/// The newer release the background poll last saw, if any.
pub fn available() -> Option<UpdateInfo> {
    AVAILABLE.lock().unwrap().clone()
}

/// Check GitHub on a worker thread — once shortly after launch (if `check_on_start`), then
/// every `interval_mins` for as long as the process runs (0 = no periodic re-check). Posts
/// `msg` to `hwnd` whenever a newer, non-skipped release is found. That post only raises a
/// passive notice: nothing downloads or restarts until a human presses Update in Settings —
/// an unattended restart (or even a modal box in front of the game) gets characters killed.
pub fn start_periodic(hwnd: isize, msg: u32, check_on_start: bool, interval_mins: u32) {
    std::thread::spawn(move || {
        if check_on_start {
            // Delayed slightly so the check never competes with window creation on a slow machine.
            std::thread::sleep(std::time::Duration::from_secs(3));
            try_check_and_post(hwnd, msg);
        }
        if interval_mins == 0 {
            return;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(interval_mins as u64 * 60));
            try_check_and_post(hwnd, msg);
        }
    });
}

fn try_check_and_post(hwnd: isize, msg: u32) {
    if prompt_active() {
        return; // an install is in flight; don't talk over it
    }
    let skip = crate::config::cached_config().update_skip_version;
    match check() {
        Ok(Some(info)) => {
            if info.version == skip {
                println!("[Cadence] Update {} available but skipped by config.", info.version);
                return;
            }
            println!("[Cadence] Update available: {} -> {}", VERSION, info.version);
            *AVAILABLE.lock().unwrap() = Some(info.clone());
            set_pending(info);
            let notified = hwnd != 0
                && msg != 0
                && unsafe { PostMessageW(hwnd as _, msg, 0, 0) != 0 };
            if !notified {
                println!("[Cadence] Couldn't notify the UI about the update.");
            }
        }
        Ok(None) => println!("[Cadence] Up to date ({}).", VERSION),
        Err(e) => println!("[Cadence] Update check failed: {}", e),
    }
}

/// Ask GitHub for the latest release. `Ok(None)` means this build is already current.
pub fn check() -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let (status, body) = https_get(&url)?;
    if status != 200 {
        return Err(format!("GitHub API returned HTTP {}", status));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("Bad API response: {}", e))?;
    parse_release(&json, VERSION)
}

/// Pull the fields we need out of a `releases/latest` payload and compare against `current`.
/// Split out from `check` so it can be unit-tested without a network call.
fn parse_release(json: &serde_json::Value, current: &str) -> Result<Option<UpdateInfo>, String> {
    let tag = json["tag_name"].as_str().unwrap_or_default();
    let version = tag.trim_start_matches('v').trim();
    let remote = parse_version(version).ok_or_else(|| format!("Unreadable release tag {:?}", tag))?;
    let local = parse_version(current).ok_or_else(|| format!("Unreadable local version {:?}", current))?;
    if remote <= local {
        return Ok(None); // strictly-greater only, so a local dev build never nags
    }
    // Pick the bare exe, not the installer that sits in the same release.
    let asset = json["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"].as_str() == Some(ASSET)))
        .ok_or_else(|| format!("Release {} has no {} asset", tag, ASSET))?;
    let url = asset["browser_download_url"].as_str().unwrap_or_default().to_string();
    if !url.starts_with(ASSET_URL_PREFIX) {
        return Err(format!("Unexpected download URL {:?}", url));
    }
    Ok(Some(UpdateInfo {
        version: version.to_string(),
        notes: json["body"].as_str().unwrap_or_default().trim().to_string(),
        url,
        size: asset["size"].as_u64().unwrap_or(0),
    }))
}

/// "3.10.0" -> (3, 10, 0). Compared as numbers so 3.10 sorts above 3.9, unlike a string compare.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.split('.');
    let major = it.next()?.trim().parse().ok()?;
    let minor = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    // Tolerate a trailing suffix on the patch component (e.g. "0-beta1").
    let patch_raw = it.next().unwrap_or("0");
    let digits: String = patch_raw.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    let patch = digits.parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// Where the running exe lives, and the two scratch names used by the swap.
fn exe_paths() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Can't locate cadence.exe: {}", e))?;
    let new = exe.with_extension("new.exe");
    let old = exe.with_extension("old.exe");
    Ok((exe, new, old))
}

/// Delete the exe an update parked aside, plus any half-written download from a failed attempt.
///
/// Runs on a background thread with a short retry: right after an update the instance we replaced
/// is often still shutting down, and Windows keeps a running image locked, so a single attempt
/// tends to fail and leave the file sitting there until the launch after next.
pub fn cleanup_old() {
    let (new, old) = match exe_paths() {
        Ok((_, new, old)) => (new, old),
        Err(_) => return,
    };
    std::thread::spawn(move || {
        for _ in 0..10 {
            if !old.exists() && !new.exists() {
                return;
            }
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::remove_file(&new);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    });
}

/// Is the folder holding cadence.exe writable? If not, an in-place swap is impossible (someone
/// installed it under Program Files) and the caller should fall back to the releases page.
pub fn can_self_update() -> bool {
    match exe_paths() {
        Ok((exe, _, _)) => {
            let probe = exe.with_extension("wtest");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        }
        Err(_) => false,
    }
}

/// Download the release and swap it in for the running exe, then relaunch. On success the caller
/// must quit promptly — the new instance is already starting.
///
/// The swap works because Windows lets a running image be *renamed* within its directory; only
/// deleting or overwriting it is blocked. So: write the new exe alongside, rename ourselves out of
/// the way, move the new one into place, relaunch, exit. If the second rename fails the first is
/// undone, leaving a working install.
pub fn apply(info: &UpdateInfo) -> Result<(), String> {
    let (exe, new, old) = exe_paths()?;
    let bytes = download_verified(info)?;

    let _ = std::fs::remove_file(&old); // a stale one would block the rename below
    std::fs::write(&new, &bytes).map_err(|e| format!("Can't write {}: {}", new.display(), e))?;
    swap_files(&exe, &new, &old)?;

    std::process::Command::new(&exe).spawn().map_err(|e| {
        // The new exe is installed but wouldn't start; leave it in place, the user can run it.
        format!("Updated to {} but couldn't relaunch: {}", info.version, e)
    })?;
    Ok(())
}

/// Move `new` into `exe`'s place, parking the current build at `old`. `new` must already exist.
///
/// Kept separate from `apply` (and free of any network or current_exe dependency) because this is
/// the step that could leave a user with no working exe, so it is unit-tested on both paths: if
/// the second rename fails the first is undone, restoring the original.
fn swap_files(
    exe: &std::path::Path,
    new: &std::path::Path,
    old: &std::path::Path,
) -> Result<(), String> {
    std::fs::rename(exe, old).map_err(|e| {
        let _ = std::fs::remove_file(new);
        format!("Can't move the current exe aside: {}", e)
    })?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(old, exe); // put the working build back
        let _ = std::fs::remove_file(new);
        return Err(format!("Can't move the new exe into place: {}", e));
    }
    Ok(())
}

/// Fetch the asset and refuse anything that isn't the exe we expect. We are about to execute this,
/// so every one of these checks matters: a captive portal or proxy error page is a plausible
/// response body, and it must never reach the disk as cadence.exe.
fn download_verified(info: &UpdateInfo) -> Result<Vec<u8>, String> {
    if !info.url.starts_with(ASSET_URL_PREFIX) {
        return Err(format!("Refusing to download from {:?}", info.url));
    }
    let (status, bytes) = https_get(&info.url)?;
    if status != 200 {
        return Err(format!("Download returned HTTP {}", status));
    }
    if info.size != 0 && bytes.len() as u64 != info.size {
        return Err(format!(
            "Download is {} bytes, expected {} — aborted.",
            bytes.len(),
            info.size
        ));
    }
    if bytes.len() < MIN_EXE_BYTES {
        return Err(format!("Download is only {} bytes — not a Cadence build.", bytes.len()));
    }
    if bytes.first() != Some(&b'M') || bytes.get(1) != Some(&b'Z') {
        return Err("Download isn't a Windows program (bad header) — aborted.".to_string());
    }
    Ok(bytes)
}

// --- WinHTTP -----------------------------------------------------------------------------------

/// RAII for the three WinHTTP handles, so every early return closes them.
struct Handle(HINTERNET);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// Split "https://host/path" into (host, path). Only HTTPS is accepted.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| format!("Not an HTTPS URL: {}", url))?;
    match rest.find('/') {
        Some(i) => Ok((rest[..i].to_string(), rest[i..].to_string())),
        None => Ok((rest.to_string(), "/".to_string())),
    }
}

/// HTTPS GET returning (status, body). Redirects are followed (WinHTTP's default), which the
/// release asset URL relies on — github.com hands off to objects.githubusercontent.com.
fn https_get(url: &str) -> Result<(u32, Vec<u8>), String> {
    let (host, path) = split_url(url)?;
    // GitHub's API rejects requests with no User-Agent.
    let agent = wide(&format!("Cadence/{}", VERSION));
    unsafe {
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen failed (no network stack?)".into());
        }
        // resolve / connect / send / receive, in ms. Generous enough for a slow link, short
        // enough that a dead network doesn't hold the worker thread for minutes.
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 20_000, 60_000 as c_int);

        let conn = Handle(WinHttpConnect(
            session.0,
            wide(&host).as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if conn.0.is_null() {
            return Err(format!("Can't reach {}", host));
        }

        let req = Handle(WinHttpOpenRequest(
            conn.0,
            wide("GET").as_ptr(),
            wide(&path).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }

        let headers = wide("Accept: application/vnd.github+json\r\n");
        if WinHttpSendRequest(req.0, headers.as_ptr(), u32::MAX, std::ptr::null_mut(), 0, 0, 0) == 0 {
            return Err(format!("Request to {} failed (offline?)", host));
        }
        if WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
            return Err(format!("No response from {}", host));
        }

        let mut status: DWORD = 0;
        let mut len = std::mem::size_of::<DWORD>() as DWORD;
        if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut _ as LPVOID,
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err("Can't read the HTTP status".into());
        }

        let mut body: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut avail: DWORD = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 {
                return Err("Transfer interrupted".into());
            }
            if avail == 0 {
                break;
            }
            let want = (avail as usize).min(chunk.len()) as DWORD;
            let mut read: DWORD = 0;
            if WinHttpReadData(req.0, chunk.as_mut_ptr() as LPVOID, want, &mut read) == 0 {
                return Err("Read failed mid-transfer".into());
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_EXE_BYTES {
                return Err("Response is implausibly large — aborted.".into());
            }
        }
        Ok((status, body))
    }
}

/// HTTPS POST of a JSON body, returning (status, response body). Same WinHTTP plumbing as
/// `https_get`; used by `report.rs` to push heartbeats to the fleet dashboard. The response
/// read is capped small — the dashboard replies `{"ok":true}`, never anything sizable.
pub(crate) fn https_post_json(url: &str, token: &str, body: &[u8]) -> Result<(u32, Vec<u8>), String> {
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    let (host, path) = split_url(url)?;
    let agent = wide(&format!("Cadence/{}", VERSION));
    unsafe {
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen failed (no network stack?)".into());
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 20_000, 30_000 as c_int);

        let conn = Handle(WinHttpConnect(
            session.0,
            wide(&host).as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if conn.0.is_null() {
            return Err(format!("Can't reach {}", host));
        }

        let req = Handle(WinHttpOpenRequest(
            conn.0,
            wide("POST").as_ptr(),
            wide(&path).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }

        // Headers and body must stay alive across WinHttpSendRequest; they do, as locals.
        let headers = wide(&format!(
            "Content-Type: application/json\r\nX-Auth-Token: {}\r\n",
            token
        ));
        if WinHttpSendRequest(
            req.0,
            headers.as_ptr(),
            u32::MAX,
            body.as_ptr() as LPVOID,
            body.len() as DWORD,
            body.len() as DWORD,
            0,
        ) == 0
        {
            return Err(format!("Request to {} failed (offline?)", host));
        }
        if WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
            return Err(format!("No response from {}", host));
        }

        let mut status: DWORD = 0;
        let mut len = std::mem::size_of::<DWORD>() as DWORD;
        if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut _ as LPVOID,
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err("Can't read the HTTP status".into());
        }

        let mut resp: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 8 * 1024];
        loop {
            let mut avail: DWORD = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 {
                return Err("Transfer interrupted".into());
            }
            if avail == 0 {
                break;
            }
            let want = (avail as usize).min(chunk.len()) as DWORD;
            let mut read: DWORD = 0;
            if WinHttpReadData(req.0, chunk.as_mut_ptr() as LPVOID, want, &mut read) == 0 {
                return Err("Read failed mid-transfer".into());
            }
            if read == 0 {
                break;
            }
            resp.extend_from_slice(&chunk[..read as usize]);
            if resp.len() > MAX_RESPONSE_BYTES {
                break; // whatever this is, it isn't the dashboard's ack — stop reading
            }
        }
        Ok((status, resp))
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("3.3.0"), Some((3, 3, 0)));
        assert_eq!(parse_version("3.10.2"), Some((3, 10, 2)));
        assert_eq!(parse_version("4"), Some((4, 0, 0)));
        assert_eq!(parse_version("3.4.0-beta1"), Some((3, 4, 0)));
        assert_eq!(parse_version("nightly"), None);
        // 3.10 must outrank 3.9 — the bug a string compare would introduce.
        assert!(parse_version("3.10.0") > parse_version("3.9.9"));
        assert!(parse_version("3.3.1") > parse_version("3.3.0"));
    }

    /// Shape of a real `releases/latest` payload, trimmed to the fields the updater reads.
    fn payload(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "body": "- Added the thing\r\n",
            "assets": [
                { "name": "CadenceSetup.exe", "size": 2397011,
                  "browser_download_url": format!("https://github.com/carlobaltazar/cadence/releases/download/{}/CadenceSetup.exe", tag) },
                { "name": "cadence.exe", "size": 933888,
                  "browser_download_url": format!("https://github.com/carlobaltazar/cadence/releases/download/{}/cadence.exe", tag) }
            ]
        })
    }

    #[test]
    fn picks_the_bare_exe_asset_when_newer() {
        let info = parse_release(&payload("v3.4.0"), "3.3.0").unwrap().expect("newer release");
        assert_eq!(info.version, "3.4.0");
        assert_eq!(info.size, 933888); // the exe, not the 2.4MB installer
        assert!(info.url.ends_with("/v3.4.0/cadence.exe"));
        assert_eq!(info.notes, "- Added the thing");
    }

    #[test]
    fn same_or_older_release_is_not_offered() {
        assert!(parse_release(&payload("v3.3.0"), "3.3.0").unwrap().is_none());
        assert!(parse_release(&payload("v3.2.0"), "3.3.0").unwrap().is_none());
        // A local build ahead of the published release must not be "updated" backwards.
        assert!(parse_release(&payload("v3.3.0"), "3.4.0").unwrap().is_none());
    }

    #[test]
    fn rejects_a_release_without_our_asset() {
        let mut j = payload("v3.4.0");
        j["assets"] = serde_json::json!([{ "name": "CadenceSetup.exe", "size": 1,
            "browser_download_url": "https://github.com/carlobaltazar/cadence/releases/download/v3.4.0/CadenceSetup.exe" }]);
        assert!(parse_release(&j, "3.3.0").is_err());
    }

    /// A tampered API reply must not be able to point the download somewhere else.
    #[test]
    fn rejects_an_off_host_download_url() {
        let mut j = payload("v3.4.0");
        j["assets"][1]["browser_download_url"] = serde_json::json!("https://evil.example/cadence.exe");
        assert!(parse_release(&j, "3.3.0").is_err());
    }

    /// Scratch dir for the swap tests, named per-test so they can run in parallel.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cadence_update_test_{}", tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn swap_installs_the_new_exe_and_parks_the_old() {
        let d = scratch("ok");
        let (exe, new, old) = (d.join("cadence.exe"), d.join("cadence.new.exe"), d.join("cadence.old.exe"));
        std::fs::write(&exe, b"old build").unwrap();
        std::fs::write(&new, b"new build").unwrap();

        swap_files(&exe, &new, &old).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new build");
        assert_eq!(std::fs::read(&old).unwrap(), b"old build", "previous build must be recoverable");
        assert!(!new.exists(), "scratch file should be consumed by the rename");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The failure that matters: if the new exe can't be moved into place, the user must be left
    /// with a working cadence.exe rather than an empty folder.
    #[test]
    fn swap_rolls_back_when_the_new_exe_is_missing() {
        let d = scratch("rollback");
        let (exe, new, old) = (d.join("cadence.exe"), d.join("cadence.new.exe"), d.join("cadence.old.exe"));
        std::fs::write(&exe, b"old build").unwrap();
        // `new` deliberately absent, so the second rename fails.

        assert!(swap_files(&exe, &new, &old).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old build", "original must be restored");
        assert!(!old.exists(), "nothing should be left parked after a rollback");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Live check against the real published release. Ignored by default so the suite stays
    /// offline-safe; run with `cargo test --release -- --ignored --nocapture` to verify the
    /// GitHub API contract, WinHTTP redirects and the download guards still hold.
    #[test]
    #[ignore]
    fn live_github_release_is_reachable_and_installable() {
        let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
        let (status, body) = https_get(&url).expect("api reachable");
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        println!("latest tag = {}", json["tag_name"]);
        // Pretend to be an older build so the real release is offered.
        let info = parse_release(&json, "3.2.0").unwrap().expect("3.2.0 is behind the release");
        println!("offering {} ({} bytes) from {}", info.version, info.size, info.url);
        let bytes = download_verified(&info).expect("asset downloads and passes verification");
        assert_eq!(&bytes[..2], b"MZ");
        assert_eq!(bytes.len() as u64, info.size);
    }

    #[test]
    fn splits_https_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/x/y/releases/latest").unwrap(),
            ("api.github.com".to_string(), "/repos/x/y/releases/latest".to_string())
        );
        assert_eq!(split_url("https://github.com").unwrap().1, "/");
        assert!(split_url("http://github.com/x").is_err());
    }
}
