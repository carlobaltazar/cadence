use crate::sequence::Sequence;
use std::fs;
use std::path::PathBuf;

fn sequences_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("ranify2").join("sequences");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[Cadence] Failed to create sequences dir: {}", e);
    }
    dir
}

pub fn save_sequence(seq: &Sequence) -> std::io::Result<PathBuf> {
    let dir = sequences_dir();
    let filename = sanitize_filename(&seq.name);
    let path = dir.join(format!("{}.json", filename));
    let json = serde_json::to_string_pretty(seq)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load_sequence(name: &str) -> std::io::Result<Sequence> {
    let dir = sequences_dir();
    let filename = sanitize_filename(name);
    let path = dir.join(format!("{}.json", filename));
    let json = fs::read_to_string(path)?;
    serde_json::from_str(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

pub fn list_sequences() -> std::io::Result<Vec<String>> {
    let dir = sequences_dir();
    let mut names = Vec::new();
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Some(stem) = path.file_stem() {
                    names.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

pub fn list_sequences_with_groups() -> std::io::Result<Vec<(String, Option<String>)>> {
    let names = list_sequences()?;
    let mut result = Vec::new();
    for name in names {
        let group = load_sequence(&name).map(|seq| seq.group).unwrap_or(None);
        result.push((name, group));
    }
    Ok(result)
}

pub fn delete_sequence(name: &str) -> std::io::Result<()> {
    let dir = sequences_dir();
    let filename = sanitize_filename(name);
    let path = dir.join(format!("{}.json", filename));
    fs::remove_file(path)
}

pub fn rename_sequence(old_filename: &str, new_name: &str) -> std::io::Result<()> {
    let mut seq = load_sequence(old_filename)?;
    let new_sanitized = sanitize_filename(new_name);
    let old_sanitized = sanitize_filename(old_filename);

    if new_sanitized != old_sanitized {
        let dir = sequences_dir();
        let new_path = dir.join(format!("{}.json", new_sanitized));
        if new_path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("A sequence named \"{}\" already exists", new_name),
            ));
        }
    }

    seq.name = new_name.to_string();
    save_sequence(&seq)?;

    if new_sanitized != old_sanitized {
        let dir = sequences_dir();
        let old_path = dir.join(format!("{}.json", old_sanitized));
        if old_path.exists() {
            fs::remove_file(old_path)?;
        }
    }

    Ok(())
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
