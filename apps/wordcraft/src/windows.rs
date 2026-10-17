//! Several windows (multi-document): every window is its own WordCraft process with its own
//! document, undo history and unsaved state, so a document can never be changed or lost through
//! another window.
//!
//! The processes find each other through `<settings>/instances/`: each writes `<pid>.json` with the
//! files it has open (kept current every frame), and asks another to come to the front by creating
//! `<pid>.focus`, which that process polls. A file open in one window is never opened (or saved
//! over) in a second one: Open focuses the owner instead. Entries of processes that are gone
//! (crash, kill) are ignored and swept.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use wordcraft_ui_egui::{WindowOutcome, WindowRequest};

fn dir() -> Option<PathBuf> {
    // Runs without preferences (agents' test runs) stay out of the user's registry.
    if std::env::var_os("WORDCRAFT_NO_PREFS").is_some() {
        return None;
    }
    wordcraft_control_key::settings_dir().map(|d| d.join("instances"))
}

/// The same file under one name: symlinks and `..` resolved when the file exists.
pub fn canonical(path: &str) -> String {
    Path::new(path).canonicalize().unwrap_or_else(|_| PathBuf::from(path)).to_string_lossy().to_string()
}

fn alive(pid: u32) -> bool {
    Command::new("/bin/ps").args(["-p", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn entry(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// Record the files process `pid` has open.
pub fn register(pid: u32, paths: &[String]) {
    let Some(dir) = dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(bytes) = serde_json::to_vec(&serde_json::json!({ "paths": paths })) {
        let _ = std::fs::write(entry(&dir, pid), bytes);
    }
}

/// Remove this process's entry (clean exit).
pub fn unregister() {
    if let Some(dir) = dir() {
        let pid = std::process::id();
        let _ = std::fs::remove_file(entry(&dir, pid));
        let _ = std::fs::remove_file(dir.join(format!("{pid}.focus")));
    }
}

/// Live entries of the other windows: pid and the files it has. Sweeps the dead ones.
fn others() -> Vec<(u32, Vec<String>)> {
    dir().map(|d| scan_dir(&d)).unwrap_or_default()
}

fn scan_dir(dir: &Path) -> Vec<(u32, Vec<String>)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let me = std::process::id();
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(pid) = name.strip_suffix(".json").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        if !alive(pid) {
            let _ = std::fs::remove_file(e.path());
            let _ = std::fs::remove_file(dir.join(format!("{pid}.focus")));
            continue;
        }
        let paths = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("paths").and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()))
            .unwrap_or_default();
        out.push((pid, paths));
    }
    out
}

/// How many other windows are open, for cascading a new window's position.
pub fn other_count() -> usize {
    others().len()
}

fn owner(path: &str) -> Option<u32> {
    let want = canonical(path);
    others().into_iter().find(|(_, paths)| paths.contains(&want)).map(|(pid, _)| pid)
}

fn request_focus(pid: u32) {
    if let Some(dir) = dir() {
        let _ = std::fs::write(dir.join(format!("{pid}.focus")), b"");
    }
}

/// Another window asked this one to come to the front.
pub fn take_focus_request() -> bool {
    dir().is_some_and(|d| std::fs::remove_file(d.join(format!("{}.focus", std::process::id()))).is_ok())
}

/// Start another WordCraft process, optionally with a file. It never inherits the control port (a
/// second server on the same port would fail) and registers the file for itself straight away, so
/// two quick opens of one file don't start two windows.
fn spawn(path: Option<&str>) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("couldn't start a new window: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.env_remove("WORDCRAFT_CONTROL_PORT").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(p) = path {
        cmd.arg(p);
    }
    let child = cmd.spawn().map_err(|e| format!("couldn't start a new window: {e}"))?;
    let pid = child.id();
    if let Some(p) = path {
        register(pid, &[canonical(p)]);
    }
    // The child outlives this call; reap it on a thread so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(pid)
}

/// The `Services::windows` hook.
pub fn host(request: WindowRequest<'_>) -> WindowOutcome {
    match request {
        WindowRequest::New => spawn(None).map_or_else(WindowOutcome::Failed, |_| WindowOutcome::Spawned),
        WindowRequest::Open(path) => match owner(path) {
            Some(pid) => {
                request_focus(pid);
                WindowOutcome::Focused
            }
            None => spawn(Some(path)).map_or_else(WindowOutcome::Failed, |_| WindowOutcome::Spawned),
        },
        WindowRequest::Owned(path) => owner(path).map_or(WindowOutcome::OpenHere, |_| WindowOutcome::Focused),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn registry_finds_the_owner_and_ignores_dead_processes() {
        let d = std::env::temp_dir().join(format!("wc-instances-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("a.docx");
        std::fs::write(&file, b"x").unwrap();
        let f = canonical(file.to_str().unwrap());
        // A live "other window": the parent shell of the test run.
        let live = std::os::unix::process::parent_id();
        std::fs::write(d.join(format!("{live}.json")), serde_json::to_vec(&serde_json::json!({"paths": [f]})).unwrap()).unwrap();
        // A dead one (pid far above any real one).
        std::fs::write(d.join("4294967.json"), serde_json::to_vec(&serde_json::json!({"paths": [f]})).unwrap()).unwrap();
        let found = scan_dir(&d);
        assert_eq!(found.iter().filter(|(_, p)| p.contains(&f)).count(), 1, "{found:?}");
        assert!(!d.join("4294967.json").exists(), "dead entry swept");
        let _ = std::fs::remove_dir_all(&d);
    }
}
