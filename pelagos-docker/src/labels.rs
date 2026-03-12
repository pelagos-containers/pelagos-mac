//! Sidecar label store: ~/.local/share/pelagos/shim-labels.json
//!
//! Maps container name → label map. Written on `docker run --label`,
//! read on `docker inspect`, cleaned up on `docker rm`.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

pub type Labels = HashMap<String, String>;

fn labels_path() -> io::Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("pelagos")
    } else {
        let home = std::env::var("HOME")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "$HOME not set"))?;
        PathBuf::from(home).join(".local/share/pelagos")
    };
    std::fs::create_dir_all(&base)?;
    Ok(base.join("shim-labels.json"))
}

fn load_all() -> io::Result<HashMap<String, Labels>> {
    let path = labels_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

fn save_all(map: &HashMap<String, Labels>) -> io::Result<()> {
    let path = labels_path()?;
    let tmp = path.with_extension("json.tmp");
    let json =
        serde_json::to_string(map).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Store labels for a container. No-op if labels is empty.
pub fn set(container: &str, labels: Labels) -> io::Result<()> {
    if labels.is_empty() {
        return Ok(());
    }
    let mut map = load_all()?;
    map.insert(container.to_string(), labels);
    save_all(&map)
}

/// Retrieve labels for a container (empty map if none stored).
pub fn get(container: &str) -> Labels {
    load_all()
        .ok()
        .and_then(|mut m| m.remove(container))
        .unwrap_or_default()
}

/// Remove label entry for a container (called on `docker rm`).
pub fn remove(container: &str) {
    if let Ok(mut map) = load_all() {
        if map.remove(container).is_some() {
            let _ = save_all(&map);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize all tests that mutate XDG_DATA_HOME (global process state).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_home<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let tmp =
            std::env::temp_dir().join(format!("pelagos-shim-test-{}-{}", std::process::id(), ns));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("XDG_DATA_HOME", &tmp);
        f();
        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn set_get_remove() {
        with_temp_home(|| {
            let mut labels = Labels::new();
            labels.insert("foo".into(), "bar".into());
            set("mybox", labels.clone()).unwrap();
            assert_eq!(get("mybox"), labels);
            remove("mybox");
            assert!(get("mybox").is_empty());
        });
    }

    #[test]
    fn get_missing_returns_empty() {
        with_temp_home(|| {
            assert!(get("nonexistent").is_empty());
        });
    }
}
