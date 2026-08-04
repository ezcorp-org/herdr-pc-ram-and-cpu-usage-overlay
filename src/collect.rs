//! Snapshot → spaces, worktree grouping, and CPU/RAM measurement.
//!
//! [`collect_spaces`] turns one `session.snapshot` (plus a `process_info` per
//! pane) into [`Space`]s. [`group_worktree_families`] and [`aggregate_families`]
//! fold worktree-child workspaces into their parent. [`measure`] samples `/proc`
//! CPU jiffies over a window and fills cpu/ram/proc counts. [`snapshot`] is the
//! top-level `collect → group → measure → aggregate` pipeline.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::herdr::Herdr;
use crate::model::{PaneInfo, Space};
use crate::proc;

/// Pseudo-agent label used to mark our agents-panel entries (agents-panel mode)
/// and to recognise / clean them up in sidebar mode.
pub const PSEUDO_AGENT: &str = "usage";

/// How one workspace's panes divide up: the cwd the branch is read from, plus
/// the agent / spare / pseudo buckets.
///
/// Split out of [`collect_spaces`] so the classification rules are unit-testable
/// without a live herdr.
#[derive(Debug, Default, PartialEq, Eq)]
struct PaneRoles {
    /// cwd of the first pane that reports one — the branch lookup path.
    cwd: Option<String>,
    /// panes with a real agent.
    agent_panes: Vec<String>,
    /// plain shell panes.
    spare_panes: Vec<String>,
    /// panes already carrying our "usage" pseudo-agent.
    pseudo_panes: Vec<String>,
}

/// Classify one workspace's panes, in the order herdr reported them.
///
/// The first pane with a non-empty cwd wins the branch lookup. `cwd` is optional
/// in herdr's payload and can be briefly absent while a pane is still starting,
/// so a later pane is allowed to supply it. Bucketing: our pseudo-agent first,
/// then any other non-empty agent, else a plain shell pane — EXCEPT panes owned
/// by another plugin ([`is_plugin_pane`]), which are never status hosts. They
/// still count toward the space's usage: measurement runs off `roots`, which
/// covers every pane regardless of these buckets.
fn classify_panes(panes: &[&PaneInfo]) -> PaneRoles {
    let mut roles = PaneRoles::default();
    for pane in panes {
        if roles.cwd.is_none() {
            if let Some(c) = pane.cwd.as_deref().filter(|c| !c.is_empty()) {
                roles.cwd = Some(c.to_string());
            }
        }
        match pane.agent.as_deref() {
            Some(PSEUDO_AGENT) => roles.pseudo_panes.push(pane.pane_id.clone()),
            Some(agent) if !agent.is_empty() => roles.agent_panes.push(pane.pane_id.clone()),
            _ if is_plugin_pane(pane) => {} // measured via roots, never a status host
            _ => roles.spare_panes.push(pane.pane_id.clone()),
        }
    }
    roles
}

/// Whether an agent-less pane belongs to another plugin: it carries metadata
/// tokens other than our own `usage` token. Plugin panes (herdr-sidebar's
/// explorer/git pane, llmtrim's dashboards, ..) stamp identity/heartbeat tokens
/// on themselves; claiming our pseudo-agent there put a second "agent" row in
/// the panel and pinned the usage text to the wrong pane. Our own fall-through
/// `usage` token must NOT trip this, or a pane we reported once would stop
/// being a spare pane on the next cycle.
///
/// A HEURISTIC, and deliberately so: herdr's `PaneInfo.tokens` is one flat
/// `map<string,string>` merged across every reporting source, with no ownership
/// field anywhere on the pane, so "someone else annotated this pane" is the
/// strongest signal the API offers. Two known costs, both preferred to grabbing
/// a pane that isn't ours:
///
/// - a plugin that badges PLAIN SHELL panes rather than opening its own shrinks
///   the spare pool; if it badges every agent-less pane in a workspace, that
///   space gets no dedicated row and its usage rides on an agent pane instead
///   (see the fall-through in [`crate::daemon::push_statuses`]);
/// - a plugin that names a token `usage` reads as a spare pane here.
fn is_plugin_pane(pane: &PaneInfo) -> bool {
    pane.tokens
        .as_ref()
        .is_some_and(|tokens| tokens.keys().any(|key| key != PSEUDO_AGENT))
}

/// Bucket a snapshot's flat pane list under each pane's `workspace_id`, keeping
/// herdr's reported order within every workspace — that order is what makes
/// "the first pane's cwd" well-defined.
fn panes_by_workspace(panes: &[PaneInfo]) -> HashMap<&str, Vec<&PaneInfo>> {
    let mut by_workspace: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
    for pane in panes {
        by_workspace
            .entry(pane.workspace_id.as_str())
            .or_default()
            .push(pane);
    }
    by_workspace
}

/// Enumerate spaces and the root shell PID of each of their panes, classifying
/// panes into agent / spare / pseudo buckets.
///
/// One `session.snapshot` supplies both the workspaces and every pane, so the
/// two can never disagree — previously a workspace closing between
/// `workspace.list` and its `pane.list` failed the whole sample. Only
/// `pane.process_info` is still per-pane (no bulk form); a pane that closed
/// mid-scan errors there and simply contributes no root.
pub fn collect_spaces(client: &mut Herdr) -> crate::Result<Vec<Space>> {
    let snapshot = client.session_snapshot()?;
    let by_workspace = panes_by_workspace(&snapshot.panes);

    let mut spaces = Vec::with_capacity(snapshot.workspaces.len());
    for ws in &snapshot.workspaces {
        let panes: &[&PaneInfo] = by_workspace
            .get(ws.workspace_id.as_str())
            .map_or(&[], Vec::as_slice);
        let roles = classify_panes(panes);

        // Best-effort shell PIDs; a pane that just closed errors and is skipped.
        let mut roots = Vec::with_capacity(panes.len());
        for pane in panes {
            if let Ok(info) = client.process_info(&pane.pane_id) {
                if let Some(pid) = info.shell_pid.filter(|&p| p != 0) {
                    roots.push(pid);
                }
            }
        }

        let label = if ws.label.is_empty() {
            ws.workspace_id.clone()
        } else {
            ws.label.clone()
        };
        let branch = git_branch(roles.cwd.as_deref());

        spaces.push(Space {
            id: ws.workspace_id.clone(),
            label,
            focused: ws.focused,
            pane_count: panes.len(),
            branch,
            roots,
            agent_panes: roles.agent_panes,
            spare_panes: roles.spare_panes,
            pseudo_panes: roles.pseudo_panes,
            ..Default::default()
        });
    }
    Ok(spaces)
}

/// git branch of `cwd` via `git -C <cwd> rev-parse --abbrev-ref HEAD`
/// (empty string if `cwd` is `None`/empty or not a repo — a non-zero git exit is
/// swallowed).
///
/// Two field choices here are deliberate; both were measured, so don't
/// "modernize" either:
///
/// - `cwd`, not `foreground_cwd`. The latter is the one #1838/#2206 made
///   non-blocking, and it transiently reports `/` on a fresh pane. `cwd` is the
///   PTY's own tracked directory and was never observed empty.
/// - the pane cwd, not the `worktree` block on `workspace.list`. That block
///   exists in 0.7.5 and 0.8.0 alike, but it can stay `null` indefinitely for a
///   workspace that IS a repo (observed on two, while `worktree.list` succeeded
///   on both), so the branch would blank out.
pub fn git_branch(cwd: Option<&str>) -> String {
    let cwd = match cwd {
        Some(c) if !c.is_empty() => c,
        _ => return String::new(),
    };
    let output = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Tag worktree-child spaces with their group parent, one `worktree.list` per
/// unique repo. Children whose repo's main checkout is open get `family_parent`.
///
/// `worktree.list` errors for non-repo workspaces; that error is folded into
/// "leave it standalone". Parent/child resolution is done against an id→index
/// map and applied after the query loop so we never hold a `&mut` into `spaces`
/// while borrowing `client`.
pub fn group_worktree_families(client: &mut Herdr, spaces: &mut [Space]) {
    let index_of: HashMap<String, usize> = spaces
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.clone(), i))
        .collect();
    let ids: Vec<String> = spaces.iter().map(|s| s.id.clone()).collect();

    let mut seen_repos: HashSet<String> = HashSet::new();
    let mut assignments: Vec<(usize, String)> = Vec::new(); // (child index, parent id)

    for ws_id in &ids {
        let res = match client.worktree_list(ws_id) {
            Ok(res) => res,
            Err(_) => continue, // workspace isn't in a git repo
        };
        let repo_key = res.source.repo_key;
        if repo_key.is_empty() || seen_repos.contains(&repo_key) {
            continue;
        }
        seen_repos.insert(repo_key);

        // The family only forms when the repo's main checkout is itself open.
        let parent_id = match res.source.source_workspace_id {
            Some(id) if index_of.contains_key(&id) => id,
            _ => continue, // main checkout isn't open — children stay standalone
        };
        for wt in res.worktrees {
            if let Some(child_id) = wt.open_workspace_id {
                if let Some(&child_idx) = index_of.get(&child_id) {
                    if child_id != parent_id {
                        assignments.push((child_idx, parent_id.clone()));
                    }
                }
            }
        }
    }

    for (child_idx, parent_id) in assignments {
        spaces[child_idx].family_parent = Some(parent_id);
    }
}

/// Sample CPU over `window_ms`, then fill `cpu` / `ram_mb` / `proc_count` on each
/// space by summing over each root's `/proc` subtree.
///
/// One `/proc` scan before, sleep, one after; per space the PID set is the union
/// of every root's subtree (built from the *after* children map). CPU% is
/// `Σ max(0, Δjiffies) / CLK_TCK / elapsed_s / NPROC * 100` — a share of the
/// whole machine (0..100). RSS and process count come from that same PID set.
pub fn measure(spaces: &mut [Space], window_ms: u64) {
    let before = proc::scan_proc();
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(window_ms));
    let after = proc::scan_proc();
    let elapsed = start.elapsed().as_secs_f64();
    let kids = proc::children_map(&after);

    let clk_tck = proc::clk_tck() as f64;
    let nproc = proc::nproc() as f64;

    for sp in spaces.iter_mut() {
        let mut pids: HashSet<u32> = HashSet::new();
        for &root in &sp.roots {
            pids.extend(proc::subtree(root, &kids));
        }

        let mut delta_jiffies: u64 = 0;
        for &pid in &pids {
            if let (Some(a), Some(b)) = (after.get(&pid), before.get(&pid)) {
                // `saturating_sub` clamps at zero, guarding counter resets and
                // pid reuse inside the window.
                delta_jiffies += a.jiffies.saturating_sub(b.jiffies);
            }
        }

        sp.cpu = if elapsed > 0.0 {
            100.0 * (delta_jiffies as f64 / clk_tck) / elapsed / nproc
        } else {
            0.0
        };
        sp.ram_mb = proc::rss_mb(&pids);
        sp.proc_count = pids.len();
    }
}

/// Fold measured worktree children into their parent (summing cpu/ram/procs/
/// panes and collecting labels), returning the spaces without folded children.
///
/// Iterates by index and reads each child's *current* values at fold time, so a
/// child that is itself a parent accumulates before contributing upward. Every
/// space carrying a `family_parent` is dropped from the result, even if that
/// parent was not found (a missing parent means the child is not surfaced on
/// its own).
pub fn aggregate_families(mut spaces: Vec<Space>) -> Vec<Space> {
    let index_of: HashMap<String, usize> = spaces
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.clone(), i))
        .collect();

    for i in 0..spaces.len() {
        let Some(parent_id) = spaces[i].family_parent.clone() else {
            continue;
        };
        let Some(&parent_idx) = index_of.get(&parent_id) else {
            continue;
        };
        // Snapshot the child's contribution (immutable borrow ends here) before
        // taking a `&mut` to the parent — `parent_idx != i` always, but this also
        // sidesteps the borrow checker cleanly.
        let (cpu, ram_mb, proc_count, pane_count, label) = {
            let child = &spaces[i];
            (
                child.cpu,
                child.ram_mb,
                child.proc_count,
                child.pane_count,
                child.label.clone(),
            )
        };
        let parent = &mut spaces[parent_idx];
        parent.cpu += cpu;
        parent.ram_mb += ram_mb;
        parent.proc_count += proc_count;
        parent.pane_count += pane_count;
        parent
            .worktree_labels
            .get_or_insert_with(Vec::new)
            .push(label);
    }

    spaces.retain(|s| s.family_parent.is_none());
    spaces
}

/// Full pipeline: collect → group worktrees → measure (`window_ms`) → aggregate.
pub fn snapshot(client: &mut Herdr, window_ms: u64) -> crate::Result<Vec<Space>> {
    let mut spaces = collect_spaces(client)?;
    group_worktree_families(client, &mut spaces);
    measure(&mut spaces, window_ms);
    Ok(aggregate_families(spaces))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PaneInfo, Space};

    /// Build a [`PaneInfo`] as `session.snapshot` reports one.
    fn pane(pane_id: &str, workspace_id: &str, cwd: Option<&str>, agent: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            workspace_id: workspace_id.to_string(),
            cwd: cwd.map(str::to_string),
            agent: agent.map(str::to_string),
            tokens: None,
        }
    }

    /// A [`PaneInfo`] carrying metadata tokens with the given keys.
    fn pane_with_tokens(pane_id: &str, keys: &[&str]) -> PaneInfo {
        let mut p = pane(pane_id, "w1", None, None);
        p.tokens = Some(
            keys.iter()
                .map(|k| (k.to_string(), "x".to_string()))
                .collect(),
        );
        p
    }

    // ---- pane bucketing + classification ------------------------------------

    #[test]
    fn panes_bucket_under_their_workspace_in_reported_order() {
        let panes = vec![
            pane("w1:p1", "w1", None, None),
            pane("w2:p1", "w2", None, None),
            pane("w1:p2", "w1", None, None),
        ];
        let by_workspace = panes_by_workspace(&panes);

        let w1: Vec<&str> = by_workspace["w1"].iter().map(|p| &*p.pane_id).collect();
        assert_eq!(w1, ["w1:p1", "w1:p2"], "order within a workspace is kept");
        assert_eq!(by_workspace["w2"].len(), 1);
        assert!(!by_workspace.contains_key("w3"));
    }

    #[test]
    fn classify_takes_the_first_non_empty_cwd() {
        // A pane can report no cwd (or an empty one) while it is still starting,
        // so the branch lookup falls through to the next pane that has one.
        let panes = [
            pane("p1", "w1", None, None),
            pane("p2", "w1", Some(""), None),
            pane("p3", "w1", Some("/repo"), None),
            pane("p4", "w1", Some("/other"), None),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        assert_eq!(classify_panes(&refs).cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn classify_splits_pseudo_agent_and_spare_panes() {
        let panes = [
            pane("p1", "w1", None, Some(PSEUDO_AGENT)),
            pane("p2", "w1", None, Some("claude")),
            pane("p3", "w1", None, None),
            pane("p4", "w1", None, Some("")), // empty agent is a plain shell
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let roles = classify_panes(&refs);

        assert_eq!(roles.pseudo_panes, ["p1"]);
        assert_eq!(roles.agent_panes, ["p2"]);
        assert_eq!(roles.spare_panes, ["p3", "p4"]);
        assert_eq!(roles.cwd, None);
    }

    #[test]
    fn classify_of_no_panes_is_all_empty() {
        assert_eq!(classify_panes(&[]), PaneRoles::default());
    }

    #[test]
    fn classify_never_offers_another_plugins_pane_as_spare() {
        // The herdr-sidebar pane: no agent, but identity/heartbeat tokens. It
        // must not become the pseudo-agent host — that is the pane-grab that
        // put a second "agent" row in the panel.
        let panes = [
            pane_with_tokens("sidebar", &["herdr-sidebar-explorer", "herdr-sidebar-git"]),
            pane("shell", "w1", None, None),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let roles = classify_panes(&refs);

        assert_eq!(roles.spare_panes, ["shell"]);
        assert!(roles.pseudo_panes.is_empty());
        assert!(roles.agent_panes.is_empty());
    }

    #[test]
    fn classify_keeps_a_pane_carrying_only_our_usage_token_as_spare() {
        // The metadata fall-through pushes the `usage` token onto a spare pane
        // WITHOUT claiming an agent; on the next cycle that pane must still be
        // a spare pane, or the status would hop panes forever.
        let panes = [pane_with_tokens("shell", &[PSEUDO_AGENT])];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        assert_eq!(classify_panes(&refs).spare_panes, ["shell"]);
    }

    #[test]
    fn classify_prefers_the_real_agent_bucket_over_plugin_detection() {
        // An agent pane with plugin badges (e.g. llmtrim tokens on a claude
        // pane) stays an agent pane — the token check only gates spares.
        let mut p = pane_with_tokens("claude", &["llmtrim"]);
        p.agent = Some("claude".to_string());
        let refs: Vec<&PaneInfo> = [&p].to_vec();
        assert_eq!(classify_panes(&refs).agent_panes, ["claude"]);
    }

    #[test]
    fn classify_misreads_a_foreign_usage_token_as_ours() {
        // KNOWN LIMITATION, pinned deliberately rather than left as prose.
        //
        // herdr merges every plugin's tokens into one flat map and does not say
        // who wrote what, so `usage` from another plugin is indistinguishable
        // from ours and its pane is offered as a spare — the pane-grab this
        // guard otherwise prevents. Closing it means renaming our token, which
        // silently blanks the sidebar of everyone whose herdr config says
        // `$usage`; with the plugin already widely installed that trade is not
        // worth a collision no plugin has yet caused. The real fix is upstream:
        // expose on read the `source` that `pane.report_metadata` already
        // requires on write.
        //
        // If this test ever fails, the rename happened — update the README's
        // "Living alongside other plugins" section with it.
        let panes = [pane_with_tokens("someone-elses", &[PSEUDO_AGENT])];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        assert_eq!(classify_panes(&refs).spare_panes, ["someone-elses"]);
    }

    #[test]
    fn classify_treats_an_empty_token_map_as_a_plain_pane() {
        // herdr sends `tokens` as an empty object once every token on a pane has
        // expired or been cleared. "Present but empty" is not ownership.
        let panes = [pane_with_tokens("shell", &[])];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        assert_eq!(classify_panes(&refs).spare_panes, ["shell"]);
    }

    #[test]
    fn classification_drops_only_plugin_panes() {
        // The safety property behind the new arm: a pane may be left out of
        // every bucket ONLY for being plugin-owned. Anything else silently
        // vanishing would be a space losing its status host for no reason.
        let mut agent = pane("claude", "w1", None, None);
        agent.agent = Some("claude".to_string());
        let mut pseudo = pane("ours", "w1", None, None);
        pseudo.agent = Some(PSEUDO_AGENT.to_string());
        let panes = [
            agent,
            pseudo,
            pane("shell", "w1", None, None),
            pane_with_tokens("mine-only", &[PSEUDO_AGENT]),
            pane_with_tokens("theirs", &["herdr-sidebar-git"]),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let roles = classify_panes(&refs);

        let mut bucketed: Vec<&str> = roles
            .agent_panes
            .iter()
            .chain(&roles.spare_panes)
            .chain(&roles.pseudo_panes)
            .map(String::as_str)
            .collect();
        bucketed.sort_unstable();
        // Everything is placed except the one pane another plugin owns.
        assert_eq!(bucketed, ["claude", "mine-only", "ours", "shell"]);
        assert_eq!(bucketed.len() + 1, panes.len());
    }

    /// Build a measured [`Space`] with just the aggregate-relevant fields set.
    fn space(id: &str, cpu: f64, ram_mb: f64, proc_count: usize, pane_count: usize) -> Space {
        Space {
            id: id.to_string(),
            label: id.to_string(),
            cpu,
            ram_mb,
            proc_count,
            pane_count,
            ..Default::default()
        }
    }

    #[test]
    fn aggregate_folds_child_into_parent_and_drops_it() {
        let parent = space("p", 10.0, 100.0, 3, 2);
        let mut child = space("c", 5.0, 50.0, 2, 1);
        child.label = "child".to_string();
        child.family_parent = Some("p".to_string());

        let out = aggregate_families(vec![parent, child]);

        assert_eq!(out.len(), 1, "the folded child is removed");
        let p = &out[0];
        assert_eq!(p.id, "p");
        assert_eq!(p.cpu, 15.0);
        assert_eq!(p.ram_mb, 150.0);
        assert_eq!(p.proc_count, 5);
        assert_eq!(p.pane_count, 3);
        assert_eq!(p.worktree_labels, Some(vec!["child".to_string()]));
    }

    #[test]
    fn aggregate_folds_multiple_children_preserving_label_order() {
        let parent = space("p", 0.0, 0.0, 0, 1);
        let mut c1 = space("c1", 1.0, 10.0, 1, 1);
        c1.label = "one".to_string();
        c1.family_parent = Some("p".to_string());
        let mut c2 = space("c2", 2.0, 20.0, 1, 1);
        c2.label = "two".to_string();
        c2.family_parent = Some("p".to_string());

        let out = aggregate_families(vec![parent, c1, c2]);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cpu, 3.0);
        assert_eq!(out[0].ram_mb, 30.0);
        assert_eq!(out[0].pane_count, 3);
        assert_eq!(
            out[0].worktree_labels,
            Some(vec!["one".to_string(), "two".to_string()]),
        );
    }

    #[test]
    fn aggregate_drops_child_even_when_parent_is_missing() {
        // The filter is purely on `family_parent` being set, so a child whose
        // parent isn't present is still not surfaced standalone.
        let standalone = space("a", 1.0, 1.0, 1, 1);
        let mut orphan = space("o", 2.0, 2.0, 1, 1);
        orphan.family_parent = Some("ghost".to_string());

        let out = aggregate_families(vec![standalone, orphan]);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
    }

    #[test]
    fn aggregate_leaves_standalone_spaces_untouched() {
        let out = aggregate_families(vec![space("a", 4.0, 8.0, 2, 2)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].worktree_labels.is_none());
        assert_eq!(out[0].cpu, 4.0);
    }

    #[test]
    fn git_branch_empty_for_none_or_blank_cwd() {
        assert_eq!(git_branch(None), "");
        assert_eq!(git_branch(Some("")), "");
    }
}
