use crate::{gui, player, recorder, storage};
use crate::win32_helpers::lock_or_recover;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

static LISTENER_RUNNING: AtomicBool = AtomicBool::new(false);
static LISTENER_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static LISTENER_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn is_listening() -> bool {
    LISTENER_RUNNING.load(Ordering::Acquire)
}

pub fn take_listener_error() -> Option<String> {
    lock_or_recover(&LISTENER_ERROR).take()
}

pub fn start_listener(port: u16, password: Option<String>) -> Result<(), String> {
    if LISTENER_RUNNING.load(Ordering::Acquire) {
        return Err("Already listening".into());
    }

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .map_err(|e| format!("Bind failed: {}", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Non-blocking failed: {}", e))?;

    LISTENER_RUNNING.store(true, Ordering::Release);
    LISTENER_SHUTDOWN.store(false, Ordering::Release);
    *lock_or_recover(&LISTENER_ERROR) = None;

    std::thread::spawn(move || {
        println!("[Cadence] Receiver listening on port {}", port);
        loop {
            if LISTENER_SHUTDOWN.load(Ordering::Acquire) {
                break;
            }
            match listener.accept() {
                Ok((stream, addr)) => {
                    println!("[Cadence] Connection from {}", addr);
                    handle_client(stream, &password);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => {
                    eprintln!("[Cadence] Accept error: {}", e);
                    *lock_or_recover(&LISTENER_ERROR) = Some(format!("{}", e));
                    break;
                }
            }
        }
        LISTENER_RUNNING.store(false, Ordering::Release);
        println!("[Cadence] Receiver stopped.");
    });

    Ok(())
}

pub fn stop_listener() {
    LISTENER_SHUTDOWN.store(true, Ordering::Release);
}

fn handle_client(mut stream: TcpStream, expected_password: &Option<String>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_stream);

    // Authentication phase
    if let Some(ref pw) = expected_password {
        let line = match read_line(&mut reader) {
            Some(l) => l,
            None => return,
        };
        if !line.starts_with("AUTH ") || line[5..].trim() != pw.as_str() {
            let _ = stream.write_all(b"ERR auth_failed\n");
            return;
        }
        let _ = stream.write_all(b"OK\n");
    }

    // Command phase
    let line = match read_line(&mut reader) {
        Some(l) => l,
        None => return,
    };
    let response = execute_command(&line);
    let _ = stream.write_all(response.as_bytes());
}

fn execute_command(line: &str) -> String {
    let trimmed = line.trim();

    if recorder::is_recording() {
        return "ERR recording\n".to_string();
    }

    if trimmed == "STOP" {
        player::cancel_playback();
        return "OK\n".to_string();
    }

    // PLAY_QUEUE: play THIS host's current queue as-is.
    if trimmed == "PLAY_QUEUE" {
        let queue = lock_or_recover(&gui::SEQUENCE_QUEUE).clone();
        if queue.is_empty() {
            return "ERR empty_queue\n".to_string();
        }
        return play_names_as_queue(queue, None);
    }

    // PLAY_LIST <label> <name> <name> …: the sender expanded a saved queue / group into sequence
    // names; load them into this host's queue (so the Items window, loop/shuffle, resume and
    // last-played all see a normal queue) and play. Names are sanitized file stems, so
    // whitespace-separated is unambiguous.
    if let Some(rest) = trimmed.strip_prefix("PLAY_LIST ") {
        let Some((label, names)) = parse_play_list(rest) else {
            return "ERR empty_list\n".to_string();
        };
        return play_names_as_queue(names, Some(label));
    }

    if trimmed.starts_with("PLAY ") {
        let seq_name = trimmed[5..].trim();
        if seq_name.is_empty() {
            return "ERR empty_name\n".to_string();
        }
        if player::is_playing() {
            return "ERR already_playing\n".to_string();
        }
        match storage::load_sequence(seq_name) {
            Ok(seq) => {
                player::set_source(player::PlaybackSource::Sequence(seq_name.to_string()));
                player::play_sequence(seq.events);
                "OK\n".to_string()
            }
            Err(_) => "ERR not_found\n".to_string(),
        }
    } else {
        "ERR unknown_command\n".to_string()
    }
}

/// Load every named sequence and play them as one queue pass. With `label` the list REPLACES
/// this host's queue first (a saved queue / group arrived by PLAY_LIST); without it the host's
/// own queue is being played and is left alone. Missing sequences are skipped like a local
/// Play Queue; only an entirely unloadable list is an error.
fn play_names_as_queue(names: Vec<String>, label: Option<String>) -> String {
    if player::is_playing() {
        return "ERR already_playing\n".to_string();
    }
    let mut event_lists = Vec::new();
    for name in &names {
        if let Ok(seq) = storage::load_sequence(name) {
            event_lists.push(seq.events);
        }
    }
    if event_lists.is_empty() {
        return "ERR load_failed\n".to_string();
    }
    if label.is_some() {
        gui::set_queue(names.clone(), label);
    }
    player::set_source(player::PlaybackSource::Queue(names));
    player::play_queue(event_lists);
    "OK\n".to_string()
}

/// `<label> <name> <name> …` → (label, names); None unless there is a label and ≥1 name.
fn parse_play_list(rest: &str) -> Option<(String, Vec<String>)> {
    let mut parts = rest.split_whitespace();
    let label = parts.next()?.to_string();
    let names: Vec<String> = parts.map(str::to_string).collect();
    if names.is_empty() {
        return None;
    }
    Some((label, names))
}

pub fn send_command(
    host: &str,
    port: u16,
    password: Option<&str>,
    command: &str,
) -> Result<String, String> {
    let addr = format!("{}:{}", host, port);
    let mut stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .map_err(|e: std::net::AddrParseError| format!("Bad address: {}", e))?,
        Duration::from_secs(3),
    )
    .map_err(|e| format!("Connect failed: {}", e))?;

    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let read_stream = stream
        .try_clone()
        .map_err(|e| format!("Clone failed: {}", e))?;
    let mut reader = BufReader::new(read_stream);

    // Auth if needed
    if let Some(pw) = password {
        stream
            .write_all(format!("AUTH {}\n", pw).as_bytes())
            .map_err(|e| format!("Send failed: {}", e))?;
        let response = read_line(&mut reader).unwrap_or_default();
        if response.trim() != "OK" {
            return Err("Authentication failed".into());
        }
    }

    // Send command
    stream
        .write_all(format!("{}\n", command).as_bytes())
        .map_err(|e| format!("Send failed: {}", e))?;

    let response = read_line(&mut reader).unwrap_or_default();
    Ok(response.trim().to_string())
}

fn read_line<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        // Generous: PLAY_LIST carries a whole expanded queue (dozens of names) on one line.
        Ok(n) if n > 8192 => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_list_parses_label_and_names() {
        assert_eq!(
            parse_play_list("night a b a"),
            Some(("night".to_string(), vec!["a".to_string(), "b".to_string(), "a".to_string()]))
        );
        assert_eq!(parse_play_list("  night   x "), Some(("night".to_string(), vec!["x".to_string()])));
        assert_eq!(parse_play_list("night"), None);
        assert_eq!(parse_play_list(""), None);
    }
}
