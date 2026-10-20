use crate::state::{get_state, WorkspaceInfo};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Gdi::InvalidateRect;

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Signal from subscriber to poller to refresh busy dots immediately
static NEEDS_SNAPSHOT: AtomicBool = AtomicBool::new(false);

/// Background poller: runs off the UI thread
pub fn start_background_poller(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as usize;

    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut _);
        let mut cycle: u32 = 0;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(60));
            cycle += 1;

            // Fast 1ms check for active workspace (instant feel)
            let mut changed = check_active_workspace_fast();

            // Refresh busy dots immediately if an event occurred, or every ~480ms fallback
            if NEEDS_SNAPSHOT.swap(false, Ordering::Relaxed) || cycle >= 8 {
                cycle = 0;
                if fetch_and_update_state() {
                    changed = true;
                }
            }

            if changed {
                unsafe {
                    let _ = InvalidateRect(hwnd, None, BOOL(0));
                }
            }
        }
    });
}

/// Fast check: parses "Active workspace: X" from plain-text output
pub fn check_active_workspace_fast() -> bool {
    let output = Command::new("lwm")
        .args(["query", "workspace"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Active workspace:") {
                if let Ok(new_id) = rest.trim().parse::<usize>() {
                    let mut state = get_state().lock().unwrap();
                    if state.active_workspace != new_id {
                        state.active_workspace = new_id;
                        for w in state.workspaces.iter_mut() {
                            w.is_active = w.id == new_id;
                        }
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Query LeopardWM snapshot: parses NDJSON records from `lwm query workspaces`
pub fn fetch_and_update_state() -> bool {
    let output = Command::new("lwm")
        .args(["query", "workspaces"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let Ok(out) = output else {
        return false;
    };

    let mut detected_active = None;
    let mut occupied = HashSet::new();

    for line in out.stdout.lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(chunk) = serde_json::from_str::<Value>(trimmed) {
            if let Some(records) = chunk.get("records").and_then(|r| r.as_array()) {
                for rec in records {
                    let kind = rec.get("kind").and_then(|k| k.as_str()).unwrap_or("");

                    if kind == "monitor" {
                        if let Some(idx) = rec.get("active_workspace_index").and_then(|i| i.as_u64()) {
                            detected_active = Some((idx as usize) + 1);
                        }
                    } else if kind == "window" {
                        let is_sticky = rec.get("is_sticky").and_then(|s| s.as_bool()).unwrap_or(false);
                        if !is_sticky {
                            if let Some(idx) = rec.get("workspace_index").and_then(|i| i.as_u64()) {
                                occupied.insert((idx as usize) + 1);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut state = get_state().lock().unwrap();
    let current_active = detected_active.unwrap_or(state.active_workspace);

    let mut updated_workspaces = Vec::new();
    for i in 1..=9 {
        updated_workspaces.push(WorkspaceInfo {
            id: i,
            is_active: i == current_active,
            has_windows: occupied.contains(&i),
        });
    }

    let changed = state.active_workspace != current_active
        || state.workspaces.len() != updated_workspaces.len()
        || state.workspaces.iter().zip(&updated_workspaces).any(|(a, b)| {
            a.id != b.id || a.is_active != b.is_active || a.has_windows != b.has_windows
        });

    if changed {
        state.workspaces = updated_workspaces;
        state.active_workspace = current_active;
        return true;
    }

    false
}

/// Real-time subscriber (catches workspace, window, and layout changes)
pub fn start_subscriber(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as usize;

    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut _);

        // Subscribing without filters catches layout_changed when windows open, close, or move
        let mut child = match Command::new("lwm")
            .args(["subscribe"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed == "Subscribed" {
                    continue;
                }

                // If active workspace changed, redraw immediately (0ms)
                if let Ok(event_json) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(new_id) = extract_active_id(&event_json) {
                        let mut state = get_state().lock().unwrap();
                        if state.active_workspace != new_id {
                            state.active_workspace = new_id;
                            for w in state.workspaces.iter_mut() {
                                w.is_active = w.id == new_id;
                            }
                            unsafe {
                                let _ = InvalidateRect(hwnd, None, BOOL(0));
                            }
                        }
                    }
                }

                // Signal the poller thread to refresh busy dots immediately (~60ms)
                NEEDS_SNAPSHOT.store(true, Ordering::Relaxed);
            }
        }
    });
}

fn extract_active_id(v: &Value) -> Option<usize> {
    if let Some(id) = check_value_for_workspace_id(v) {
        return Some(id);
    }

    if let Some(obj) = v.as_object() {
        for key in &[
            "workspace_changed",
            "workspace",
            "active_workspace",
            "focused_workspace",
            "current_workspace",
            "data",
            "payload",
            "event",
        ] {
            if let Some(nested) = obj.get(*key) {
                if let Some(id) = check_value_for_workspace_id(nested) {
                    return Some(id);
                }
                if let Some(id) = extract_active_id(nested) {
                    return Some(id);
                }
            }
        }
    }

    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(id) = check_value_for_workspace_id(item) {
                return Some(id);
            }
        }
    }

    None
}

fn check_value_for_workspace_id(v: &Value) -> Option<usize> {
    if let Some(num) = v.as_u64() {
        return Some(if num < 9 { (num as usize) + 1 } else { num as usize });
    }

    if let Some(obj) = v.as_object() {
        for key in &["to", "new_workspace", "target", "new_index"] {
            if let Some(val) = obj.get(*key) {
                if let Some(num) = val.as_u64() {
                    return Some(if *key == "new_index" { (num as usize) + 1 } else if num < 9 { (num as usize) + 1 } else { num as usize });
                }
            }
        }

        for key in &["active_workspace", "focused_workspace", "current_workspace", "workspace"] {
            if let Some(val) = obj.get(*key) {
                if let Some(num) = val.as_u64() {
                    return Some(if num < 9 { (num as usize) + 1 } else { num as usize });
                }
                if let Some(s) = val.as_str() {
                    if let Ok(num) = s.parse::<usize>() {
                        return Some(if num < 9 { num + 1 } else { num });
                    }
                }
                if let Some(inner) = check_value_for_workspace_id(val) {
                    return Some(inner);
                }
            }
        }

        for key in &["active_workspace_index", "active_index", "workspace_index", "index"] {
            if let Some(val) = obj.get(*key) {
                if let Some(num) = val.as_u64() {
                    return Some((num as usize) + 1);
                }
            }
        }

        if let Some(name) = obj.get("name").and_then(|n| n.as_str()) {
            if let Ok(num) = name.trim().parse::<usize>() {
                if (1..=9).contains(&num) {
                    return Some(num);
                }
            }
        }
    }

    None
}

pub fn switch_to_workspace(id: usize) {
    let _ = Command::new("lwm")
        .args(["workspace", &id.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}