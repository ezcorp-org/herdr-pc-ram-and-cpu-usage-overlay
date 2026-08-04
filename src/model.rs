//! Data types shared across the plugin.
//!
//! [`Space`] is the internal per-workspace aggregate. The remaining types are
//! `serde` views over the `result` payloads of the herdr socket read methods we
//! call (`session.snapshot`, `pane.process_info`, `worktree.list`). Each only
//! declares the fields we consume; serde ignores the rest of herdr's payload.

use serde::Deserialize;

/// CPU / RAM aggregate for one herdr space (workspace).
///
/// - `roots` are the shell PIDs of each pane (process-tree roots).
/// - `cpu` / `ram_mb` / `proc_count` are filled in by the measure step.
/// - `family_parent` / `worktree_labels` are set when a worktree child is
///   folded into its parent space.
#[derive(Debug, Clone, Default)]
pub struct Space {
    /// herdr workspace id.
    pub id: String,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    /// git branch of the first pane's cwd (empty if none).
    pub branch: String,
    /// shell PIDs of each pane (process-tree roots).
    pub roots: Vec<u32>,
    /// panes with a real agent.
    pub agent_panes: Vec<String>,
    /// plain shell panes.
    pub spare_panes: Vec<String>,
    /// panes carrying our "usage" pseudo-agent.
    pub pseudo_panes: Vec<String>,
    /// CPU % of the whole machine (all cores), filled by measure.
    pub cpu: f64,
    /// RSS MB, filled by measure.
    pub ram_mb: f64,
    /// processes counted, filled by measure.
    pub proc_count: usize,
    /// workspace id of the worktree-group parent.
    pub family_parent: Option<String>,
    /// labels of folded worktree children.
    pub worktree_labels: Option<Vec<String>>,
}

// ---- session.snapshot -------------------------------------------------------
//
// result = { "type": "session_snapshot", "snapshot": { workspaces: [ .. ],
//            panes: [ .. ], tabs, layouts, agents, .. } }
//
// One call returns every workspace AND every pane, so the two can never be read
// torn apart — a workspace that closes mid-scan simply isn't in the snapshot,
// rather than making a follow-up `pane.list` fail with `workspace_not_found`.

/// `result` payload of `session.snapshot`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSnapshotResult {
    pub snapshot: SessionSnapshot,
}

/// The `snapshot` object; only the two collections we consume are modelled.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
}

/// One entry of `workspaces`; only the fields we consume are modelled.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
}

/// One entry of `panes`; only the fields we consume are modelled. `workspace_id`
/// is what buckets each pane back under its workspace. `tokens` is the pane's
/// metadata-token map (`pane.report_metadata`): panes opened by other plugins
/// stamp their identity there (e.g. herdr-sidebar's heartbeat), which is how
/// the classifier keeps its status row off them.
#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub tokens: Option<std::collections::HashMap<String, serde_json::Value>>,
}

// ---- pane.process_info ------------------------------------------------------
//
// result = { "type": "pane_process_info", "process_info": { shell_pid?, .. } }

/// `result` payload of `pane.process_info`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessInfoResult {
    pub process_info: ProcessInfo,
}

/// The `process_info` object; we only need the shell PID.
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessInfo {
    #[serde(default)]
    pub shell_pid: Option<u32>,
}

// ---- worktree.list ----------------------------------------------------------
//
// result = { "type": "worktree_list", "source": { .. }, "worktrees": [ .. ] }
// (this method ERRORS when the workspace is not a git repo)

/// `result` payload of `worktree.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeListResult {
    pub source: WorktreeSource,
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
}

/// The `source` object identifying the repo and its main checkout's workspace.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeSource {
    pub repo_key: String,
    #[serde(default)]
    pub source_workspace_id: Option<String>,
}

/// One entry of `worktrees`; only the open workspace id matters for grouping.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeEntry {
    #[serde(default)]
    pub open_workspace_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Abridged verbatim `session.snapshot` reply from herdr 0.8.0 (protocol 19),
    /// keeping the field names and nesting exactly as the server sends them. The
    /// point is to fail loudly if a herdr upgrade moves them.
    const SESSION_SNAPSHOT: &str = r#"{
      "type": "session_snapshot",
      "snapshot": {
        "version": "0.8.0",
        "protocol": 19,
        "focused_workspace_id": "wS",
        "workspaces": [
          { "workspace_id": "wS", "number": 1, "label": "EZHarness", "focused": true,
            "pane_count": 2, "tab_count": 1, "active_tab_id": "wS:t0",
            "agent_status": "working", "tokens": { "usage": "cpu 1% · ram 13%" },
            "worktree": { "repo_key": "/repo/.git", "repo_name": "repo",
                          "repo_root": "/repo", "checkout_path": "/repo",
                          "is_linked_worktree": false } },
          { "workspace_id": "wT", "number": 2, "label": "", "focused": false,
            "pane_count": 1, "tab_count": 1, "active_tab_id": "wT:t1",
            "agent_status": "unknown" }
        ],
        "tabs": [],
        "panes": [
          { "pane_id": "wS:pZ", "terminal_id": "t1", "workspace_id": "wS",
            "tab_id": "wS:t1", "focused": false, "cwd": "/repo",
            "foreground_cwd": "/repo", "agent_status": "unknown", "revision": 0 },
          { "pane_id": "wS:p3K", "terminal_id": "t2", "workspace_id": "wS",
            "tab_id": "wS:t0", "focused": true, "cwd": "/repo", "agent": "claude",
            "agent_status": "working", "revision": 1 },
          { "pane_id": "wT:p1", "terminal_id": "t3", "workspace_id": "wT",
            "tab_id": "wT:t1", "focused": false, "cwd": null,
            "agent_status": "unknown", "revision": 0,
            "tokens": { "herdr-sidebar-explorer": "1785860662",
                        "herdr-sidebar-git": "1785860662" } }
        ],
        "layouts": [],
        "agents": []
      }
    }"#;

    #[test]
    fn deserializes_a_real_session_snapshot_payload() {
        let parsed: SessionSnapshotResult = serde_json::from_str(SESSION_SNAPSHOT).unwrap();
        let snap = parsed.snapshot;

        assert_eq!(snap.workspaces.len(), 2);
        assert_eq!(snap.workspaces[0].workspace_id, "wS");
        assert_eq!(snap.workspaces[0].label, "EZHarness");
        assert!(snap.workspaces[0].focused);
        assert_eq!(snap.workspaces[1].label, "", "an empty label stays empty");
        assert!(!snap.workspaces[1].focused);

        assert_eq!(snap.panes.len(), 3);
        assert_eq!(snap.panes[0].workspace_id, "wS");
        assert_eq!(snap.panes[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(snap.panes[0].agent, None);
        assert_eq!(snap.panes[1].agent.as_deref(), Some("claude"));
        // An explicit `null` cwd must not be an error — herdr can omit it while a
        // pane is still starting.
        assert_eq!(snap.panes[2].cwd, None);
        // Plugin panes surface their identity tokens; plain panes have none.
        assert!(snap.panes[0].tokens.is_none());
        let tokens = snap.panes[2].tokens.as_ref().expect("sidebar pane tokens");
        assert!(tokens.contains_key("herdr-sidebar-explorer"));
    }

    #[test]
    fn process_info_reads_the_shell_pid_and_tolerates_its_absence() {
        let with_pid = r#"{"process_info":{"pane_id":"wS:p1","shell_pid":4094680,
            "foreground_process_group_id":4094932,"foreground_processes":[]}}"#;
        let parsed: ProcessInfoResult = serde_json::from_str(with_pid).unwrap();
        assert_eq!(parsed.process_info.shell_pid, Some(4094680));

        // `shell_pid` is nullable in herdr's schema and `pane_id` is the only
        // required field, so both shapes must parse.
        let bare: ProcessInfoResult =
            serde_json::from_str(r#"{"process_info":{"pane_id":"wS:p1"}}"#).unwrap();
        assert_eq!(bare.process_info.shell_pid, None);
    }
}
