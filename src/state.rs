use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: usize,        // 1-based index (1..=9)
    pub is_active: bool,  // Currently visible/focused
    pub has_windows: bool,
}

#[derive(Debug, Clone)]
pub struct SwitcherState {
    pub active_workspace: usize,
    pub workspaces: Vec<WorkspaceInfo>,
}

impl Default for SwitcherState {
    fn default() -> Self {
        let workspaces = (1..=9)
            .map(|id| WorkspaceInfo {
                id,
                is_active: id == 1,
                has_windows: false,
            })
            .collect();

        Self {
            active_workspace: 1,
            workspaces,
        }
    }
}

static GLOBAL_STATE: OnceLock<Mutex<SwitcherState>> = OnceLock::new();

pub fn get_state() -> &'static Mutex<SwitcherState> {
    GLOBAL_STATE.get_or_init(|| Mutex::new(SwitcherState::default()))
}