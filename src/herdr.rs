//! Socket-native herdr JSON-RPC client.
//!
//! One persistent [`UnixStream`] (paired with a [`BufReader`] over a cloned fd)
//! speaks herdr's newline-delimited JSON-RPC:
//!   request : `{"id":"<unique>","method":"<name>","params":{...}}\n`
//!   success : `{"id":..,"result":{"type":"<snake>",...}}`
//!   failure : `{"id":..,"error":{"code":..,"message":..}}`
//! Any non-`error` envelope is treated as success (mutations return
//! `{"type":"ok"}`). Every method and param name below is verified against the
//! herdr 0.8.0 API schema (`herdr` protocol 19) — reads (`SessionSnapshotParams`
//! is empty, `PaneProcessInfoParams`, `WorktreeListParams`) and mutations alike
//! (`PaneReportAgentParams`, `PaneReleaseAgentParams`, `PaneReportMetadataParams`,
//! `WorkspaceReportMetadataParams`, `NotificationShowParams`,
//! `ClientWindowTitleSetParams`). Because every param name is confirmed, all
//! methods go over the socket; [`bin_path`] still exposes the `HERDR_BIN_PATH`
//! CLI for callers that need a degraded fallback (e.g. best-effort notifications
//! without a socket).
//!
//! The socket path is resolved `HERDR_SOCKET_PATH` → `$XDG_CONFIG_HOME/herdr` →
//! `~/.config/herdr/herdr.sock` (the XDG/home resolution is reused from
//! [`crate::config`]).

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::model::{
    ProcessInfo, ProcessInfoResult, SessionSnapshot, SessionSnapshotResult, WorktreeListResult,
};

/// Read/write timeout for a single JSON-RPC round-trip. Generous — herdr answers
/// in milliseconds; this only guards against a wedged host hanging the plugin.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// A live JSON-RPC connection to the herdr host.
pub struct Herdr {
    /// Write half — requests are written here and flushed.
    stream: UnixStream,
    /// Read half — a `BufReader` over a cloned fd so we can `read_line` while the
    /// write half stays borrowable.
    reader: BufReader<UnixStream>,
    /// Socket path, retained so a broken connection can be re-opened.
    path: PathBuf,
    /// Monotonic request id counter (unique per connection, and across reconnects
    /// since it is never reset).
    next_id: u64,
}

/// Open a connection to the herdr socket.
pub fn connect() -> crate::Result<Herdr> {
    Herdr::open(socket_path()?)
}

impl Herdr {
    /// Connect to `path` and wire up the read/write halves.
    fn open(path: PathBuf) -> crate::Result<Herdr> {
        let stream = UnixStream::connect(&path)
            .map_err(|e| format!("cannot connect to herdr socket {}: {e}", path.display()))?;
        configure(&stream)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Herdr {
            stream,
            reader,
            path,
            next_id: 1,
        })
    }

    /// Re-open the socket after a broken pipe, replacing both halves.
    fn reconnect(&mut self) -> crate::Result<()> {
        let stream = UnixStream::connect(&self.path).map_err(|e| {
            format!(
                "cannot reconnect to herdr socket {}: {e}",
                self.path.display()
            )
        })?;
        configure(&stream)?;
        self.reader = BufReader::new(stream.try_clone()?);
        self.stream = stream;
        Ok(())
    }

    /// Next monotonic request id (plugin-prefixed for readability in herdr logs).
    fn next_request_id(&mut self) -> String {
        let n = self.next_id;
        self.next_id += 1;
        format!("ez-corp.space-usage:{n}")
    }

    /// Send `method`/`params`, read exactly one reply line, and return its
    /// `result` object (or surface `error.message` as an `Err`). Reconnects once
    /// and retries on an I/O failure (a broken pipe from a restarted server).
    fn call(&mut self, method: &str, params: &Value) -> crate::Result<Value> {
        let id = self.next_request_id();
        let line = match self.round_trip(&id, method, params) {
            Ok(line) => line,
            Err(_) => {
                // A dropped connection surfaces as a write EPIPE or a read EOF.
                // Re-open once and retry before giving up.
                self.reconnect()?;
                self.round_trip(&id, method, params)?
            }
        };
        parse_envelope(&line)
    }

    /// Write one framed request and read exactly one reply line back. I/O errors
    /// (including a closed connection) are returned so [`call`] can reconnect.
    fn round_trip(&mut self, id: &str, method: &str, params: &Value) -> io::Result<String> {
        let request = frame_request(id, method, params);
        self.stream.write_all(request.as_bytes())?;
        self.stream.flush()?;

        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "herdr closed the connection",
            ));
        }
        Ok(line)
    }

    // ---- read methods -------------------------------------------------------

    /// `session.snapshot` — every open workspace AND every pane in one call.
    ///
    /// Deliberately one round trip rather than `workspace.list` + a `pane.list`
    /// per workspace: those can be read torn apart, and a workspace that closes
    /// between them makes the follow-up `pane.list` fail with
    /// `workspace_not_found`, which used to abort the whole sample.
    pub fn session_snapshot(&mut self) -> crate::Result<SessionSnapshot> {
        let result = self.call("session.snapshot", &json!({}))?;
        Ok(serde_json::from_value::<SessionSnapshotResult>(result)?.snapshot)
    }

    /// `pane.process_info` — the pane's shell PID (no bulk form exists).
    pub fn process_info(&mut self, pane_id: &str) -> crate::Result<ProcessInfo> {
        let result = self.call("pane.process_info", &json!({ "pane_id": pane_id }))?;
        Ok(serde_json::from_value::<ProcessInfoResult>(result)?.process_info)
    }

    /// `worktree.list` — errors when `workspace_id` is not a git repo. That error
    /// is returned as an ordinary (recoverable) `Err`; the caller folds it into
    /// "this workspace isn't a repo — leave it standalone".
    pub fn worktree_list(&mut self, workspace_id: &str) -> crate::Result<WorktreeListResult> {
        let result = self.call("worktree.list", &json!({ "workspace_id": workspace_id }))?;
        Ok(serde_json::from_value::<WorktreeListResult>(result)?)
    }

    // ---- mutation methods ---------------------------------------------------

    /// `pane.report_agent` — claim a pseudo-agent row (agents-panel mode).
    ///
    /// herdr 0.7.5 dropped the `custom_status` field from this call; the status
    /// text now rides a named metadata token (see [`Self::report_metadata_status`]),
    /// so this only claims the agent identity + state. The caller pushes the token
    /// separately onto the same pane.
    pub fn report_agent(
        &mut self,
        pane_id: &str,
        source: &str,
        agent: &str,
        state: &str,
    ) -> crate::Result<()> {
        self.call(
            "pane.report_agent",
            &json!({
                "pane_id": pane_id,
                "source": source,
                "agent": agent,
                "state": state,
            }),
        )?;
        Ok(())
    }

    /// `pane.release_agent` — drop a previously reported pseudo-agent.
    pub fn release_agent(&mut self, pane_id: &str, source: &str, agent: &str) -> crate::Result<()> {
        self.call(
            "pane.release_agent",
            &json!({ "pane_id": pane_id, "source": source, "agent": agent }),
        )?;
        Ok(())
    }

    /// `pane.report_metadata` — set a TTL'd named `token` to `value` on a pane.
    ///
    /// herdr 0.7.5 replaced the single `custom_status` field with a `tokens` map
    /// (`{name: value|null}`); the sidebar renders it wherever the configured
    /// `[sidebar.spaces]` / `[sidebar.agents]` rows reference `$<token>`. The TTL
    /// self-clears the value if the daemon dies.
    pub fn report_metadata_status(
        &mut self,
        pane_id: &str,
        source: &str,
        token: &str,
        value: &str,
        ttl_ms: u64,
    ) -> crate::Result<()> {
        self.call(
            "pane.report_metadata",
            &json!({
                "pane_id": pane_id,
                "source": source,
                "tokens": { token: value },
                "ttl_ms": ttl_ms,
            }),
        )?;
        Ok(())
    }

    /// `pane.report_metadata` clearing a named `token` (sets it to `null`).
    pub fn clear_metadata_status(
        &mut self,
        pane_id: &str,
        source: &str,
        token: &str,
    ) -> crate::Result<()> {
        self.call(
            "pane.report_metadata",
            &json!({
                "pane_id": pane_id,
                "source": source,
                "tokens": { token: Value::Null },
            }),
        )?;
        Ok(())
    }

    /// `workspace.report_metadata` — set a TTL'd named `token` on a WORKSPACE.
    ///
    /// The **spaces card** (`[ui.sidebar.spaces]`, `$<token>`) renders
    /// *workspace* tokens; a pane token (see [`Self::report_metadata_status`])
    /// only reaches the agents panel. So sidebar mode reports at the workspace
    /// level. The TTL self-clears the value if the daemon dies.
    pub fn workspace_report_metadata(
        &mut self,
        workspace_id: &str,
        source: &str,
        token: &str,
        value: &str,
        ttl_ms: u64,
    ) -> crate::Result<()> {
        self.call(
            "workspace.report_metadata",
            &json!({
                "workspace_id": workspace_id,
                "source": source,
                "tokens": { token: value },
                "ttl_ms": ttl_ms,
            }),
        )?;
        Ok(())
    }

    /// `workspace.report_metadata` clearing a named workspace `token`.
    pub fn workspace_clear_metadata(
        &mut self,
        workspace_id: &str,
        source: &str,
        token: &str,
    ) -> crate::Result<()> {
        self.call(
            "workspace.report_metadata",
            &json!({
                "workspace_id": workspace_id,
                "source": source,
                "tokens": { token: Value::Null },
            }),
        )?;
        Ok(())
    }

    /// `client.window_title.set`.
    pub fn window_title_set(&mut self, title: &str) -> crate::Result<()> {
        self.call("client.window_title.set", &json!({ "title": title }))?;
        Ok(())
    }

    /// `client.window_title.clear`.
    pub fn window_title_clear(&mut self) -> crate::Result<()> {
        self.call("client.window_title.clear", &json!({}))?;
        Ok(())
    }

    /// `notification.show` — best-effort toast.
    pub fn notification_show(&mut self, title: &str, body: &str) -> crate::Result<()> {
        self.call(
            "notification.show",
            &json!({ "title": title, "body": body }),
        )?;
        Ok(())
    }
}

/// Resolve the herdr socket path (`HERDR_SOCKET_PATH` → XDG → `~/.config`).
pub fn socket_path() -> crate::Result<PathBuf> {
    Ok(socket_path_from(
        crate::config::non_empty_env("HERDR_SOCKET_PATH").as_deref(),
        &crate::config::config_home(),
    ))
}

/// The herdr CLI binary (`HERDR_BIN_PATH`, else `herdr`) for the fallback path.
///
/// Deliberately retained but not yet wired: every method currently goes over the
/// socket, so this degraded CLI path has no caller. Kept as the documented
/// escape hatch for a socket-unavailable fallback (see the module docs).
#[allow(dead_code)]
pub fn bin_path() -> String {
    crate::config::non_empty_env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string())
}

// ---- pure helpers (unit-tested) ---------------------------------------------

/// Serialize a request into one newline-terminated JSON line.
///
/// A dedicated struct fixes field order (`id`, `method`, `params`) independent of
/// serde_json's map feature, so framing is deterministic.
fn frame_request(id: &str, method: &str, params: &Value) -> String {
    #[derive(Serialize)]
    struct Wire<'a> {
        id: &'a str,
        method: &'a str,
        params: &'a Value,
    }
    let mut line = serde_json::to_string(&Wire { id, method, params })
        .expect("request serialization is infallible");
    line.push('\n');
    line
}

/// Parse one response line: `error` (non-null) → `Err(message)`, otherwise the
/// `result` object (or `{}` when a non-error envelope omits it).
fn parse_envelope(line: &str) -> crate::Result<Value> {
    let envelope: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("invalid herdr response: {e}"))?;
    if let Some(error) = envelope.get("error") {
        if !error.is_null() {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("herdr error");
            return Err(message.into());
        }
    }
    Ok(envelope.get("result").cloned().unwrap_or_else(|| json!({})))
}

/// Socket path from an optional `HERDR_SOCKET_PATH` override and the resolved
/// config home: the override wins, else `<config_home>/herdr/herdr.sock`.
fn socket_path_from(explicit: Option<&str>, config_home: &Path) -> PathBuf {
    match explicit {
        Some(path) => PathBuf::from(path),
        None => config_home.join("herdr").join("herdr.sock"),
    }
}

/// Apply the round-trip read/write timeouts to a freshly connected stream.
fn configure(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- request framing ----------------------------------------------------

    #[test]
    fn frames_request_as_one_json_line() {
        let line = frame_request("id-1", "pane.process_info", &json!({ "pane_id": "p1" }));
        assert!(line.ends_with('\n'));
        assert_eq!(
            line.matches('\n').count(),
            1,
            "exactly one trailing newline"
        );

        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], "id-1");
        assert_eq!(v["method"], "pane.process_info");
        assert_eq!(v["params"]["pane_id"], "p1");
    }

    #[test]
    fn frames_empty_params_as_object() {
        let line = frame_request("x", "workspace.list", &json!({}));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        // params must be present and an (empty) object, not null — the server's
        // adjacently-tagged Method enum requires the `params` field.
        assert!(v["params"].is_object());
        assert!(v["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn frames_field_order_is_id_method_params() {
        let line = frame_request("7", "m", &json!({}));
        assert!(line.starts_with(r#"{"id":"7","method":"m","params":{"#));
    }

    // ---- envelope parsing ---------------------------------------------------

    #[test]
    fn parses_result_bearing_success() {
        let result =
            parse_envelope(r#"{"id":"1","result":{"type":"workspace_list","workspaces":[]}}"#)
                .unwrap();
        assert_eq!(result["type"], "workspace_list");
        assert!(result["workspaces"].is_array());
    }

    #[test]
    fn treats_ok_mutation_as_success() {
        let result = parse_envelope(r#"{"id":"1","result":{"type":"ok"}}"#).unwrap();
        assert_eq!(result["type"], "ok");
    }

    #[test]
    fn non_error_without_result_yields_empty_object() {
        let result = parse_envelope(r#"{"id":"1"}"#).unwrap();
        assert!(result.as_object().is_some_and(|m| m.is_empty()));
    }

    #[test]
    fn null_error_is_still_success() {
        let result = parse_envelope(r#"{"id":"1","error":null,"result":{"type":"ok"}}"#).unwrap();
        assert_eq!(result["type"], "ok");
    }

    #[test]
    fn surfaces_error_message() {
        let err = parse_envelope(
            r#"{"id":"1","error":{"code":"not_a_repo","message":"workspace is not a git repository"}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a git repository"));
    }

    #[test]
    fn error_without_message_falls_back() {
        let err = parse_envelope(r#"{"id":"1","error":{"code":"boom"}}"#).unwrap_err();
        assert_eq!(err.to_string(), "herdr error");
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_envelope("not json").is_err());
    }

    // ---- socket-path resolution ---------------------------------------------

    #[test]
    fn socket_path_prefers_explicit_override() {
        let config_home = Path::new("/home/u/.config");
        assert_eq!(
            socket_path_from(Some("/run/herdr/herdr.sock"), config_home),
            PathBuf::from("/run/herdr/herdr.sock"),
        );
    }

    #[test]
    fn socket_path_falls_back_to_config_home() {
        let config_home = Path::new("/home/u/.config");
        assert_eq!(
            socket_path_from(None, config_home),
            PathBuf::from("/home/u/.config/herdr/herdr.sock"),
        );
    }
}
