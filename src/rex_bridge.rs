use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RexAgentChoice {
    pub id: String,
    pub kind: String, // "live" | "new"
    pub block_id: Option<String>,
    pub window_id: Option<String>,
    pub program_key: String,
    pub program_name: String,
    pub label: String,
    pub compact_label: String,
    pub status: String, // "ready" | "working" | "blocked" | "new"
    pub blocked_kind: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RexSessionContext {
    pub session_id: Option<String>,
    pub self_block_id: Option<String>,
    pub active_window_id: Option<String>,
}

const KNOWN_AGENTS: &[(&str, &str, &[&str])] = &[
    ("antigravity", "Antigravity", &["agy", "antigravity"]),
    ("claude", "Claude Code", &["claude", "claude-code"]),
    ("codex", "Codex", &["codex"]),
    ("opencode", "OpenCode", &["opencode"]),
    ("pi", "Pi", &["pi"]),
    ("aider", "Aider", &["aider"]),
    ("gemini", "Gemini CLI", &["gemini"]),
    ("amp", "Amp", &["amp"]),
];

pub fn find_rex_bin() -> String {
    if let Ok(b) = env::var("REX_BIN")
        && !b.is_empty()
        && Path::new(&b).is_file()
    {
        return b;
    }
    for cand in [
        "/opt/homebrew/bin/rex",
        "/usr/local/bin/rex",
        "/Applications/Rex Beta.app/Contents/Helpers/rex",
        "/Applications/Rex.app/Contents/Helpers/rex",
    ] {
        if Path::new(cand).is_file() {
            return cand.to_string();
        }
    }
    "rex".to_string()
}

fn run_rex_json(args: &[&str]) -> Option<Value> {
    let bin = find_rex_bin();
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&out.stdout).ok()
}

pub fn detect_session_context() -> RexSessionContext {
    let mut session_id = env::var("REX_SESSION").ok().filter(|s| !s.is_empty());
    let mut self_block_id = env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());

    if session_id.is_none()
        && let Some(who) = run_rex_json(&["whoami", "--json"])
    {
        session_id = who
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if self_block_id.is_none() {
            self_block_id = who
                .get("block_id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }

    let active_window_id = session_id.as_deref().and_then(|sid| {
        let inspect = run_rex_json(&["session", "inspect", sid, "--json"])?;
        inspect
            .get("active_window_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    RexSessionContext {
        session_id,
        self_block_id,
        active_window_id,
    }
}

pub fn parse_file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let slash_pos = rest.find('/')?;
    let path = &rest[slash_pos..];
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

pub fn query_block_cwd(session_id: &str, block_id: &str) -> Option<PathBuf> {
    if let Some(proc) = run_rex_json(&[
        "block", "call", "-s", session_id, "-b", block_id, "process", "{}",
    ]) {
        if let Some(cwd) = proc
            .pointer("/foreground/cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(PathBuf::from(cwd));
        }
        if let Some(ancs) = proc.get("ancestors").and_then(Value::as_array) {
            for anc in ancs {
                if let Some(cwd) = anc
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    return Some(PathBuf::from(cwd));
                }
            }
        }
    }

    if let Some(pwd_val) = run_rex_json(&[
        "block", "call", "-s", session_id, "-b", block_id, "pwd", "{}",
    ]) && let Some(pwd_url) = pwd_val.get("pwd").and_then(Value::as_str)
        && let Some(path) = parse_file_url_path(pwd_url)
    {
        return Some(PathBuf::from(path));
    }

    None
}

/// Inspects the active Rex window to find the focused non-omnidiff terminal block and its CWD.
pub fn resolve_live_followed_cwd(
    session_id: &str,
    self_block_id: Option<&str>,
    fallback_source_block: Option<&str>,
) -> Option<(String, PathBuf)> {
    let inspect = run_rex_json(&["session", "inspect", session_id, "--json"])?;
    let active_win_id = inspect.get("active_window_id").and_then(Value::as_str);
    let windows = inspect.get("windows").and_then(Value::as_array)?;

    let win = windows
        .iter()
        .find(|w| w.get("window_id").and_then(Value::as_str) == active_win_id)
        .or_else(|| windows.first())?;

    let focused_id = win.get("focused_block_id").and_then(Value::as_str);
    let mut candidate_blocks: Vec<String> = Vec::new();

    if let Some(fid) = focused_id
        && Some(fid) != self_block_id
    {
        candidate_blocks.push(fid.to_string());
    }
    if let Some(src) = fallback_source_block
        && Some(src) != self_block_id
        && !candidate_blocks.iter().any(|b| b == src)
    {
        candidate_blocks.push(src.to_string());
    }

    if let Some(layers) = win.get("layers").and_then(Value::as_array) {
        for layer in layers {
            if let Some(blocks) = layer.get("blocks").and_then(Value::as_array) {
                for blk in blocks {
                    let lbl = blk.get("label").and_then(Value::as_str).unwrap_or("");
                    if lbl == "omnidiff-float" || lbl == "omnidiff-dock" {
                        continue;
                    }
                    if let Some(bid) = blk.get("block_id").and_then(Value::as_str)
                        && Some(bid) != self_block_id
                        && !candidate_blocks.iter().any(|b| b == bid)
                    {
                        candidate_blocks.push(bid.to_string());
                    }
                }
            }
        }
    }

    for bid in candidate_blocks {
        if let Some(cwd) = query_block_cwd(session_id, &bid) {
            return Some((bid, cwd));
        }
    }
    None
}

pub fn match_agent_identity(raw_name: &str) -> Option<(&'static str, &'static str)> {
    let lower = raw_name
        .rsplit('/')
        .next()
        .unwrap_or(raw_name)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    for &(key, display, aliases) in KNOWN_AGENTS {
        if aliases.iter().any(|&a| a == lower) {
            return Some((key, display));
        }
    }
    None
}

/// Parses Rex `process` and OSC 7501 `program_status` JSON payloads for a single terminal block.
pub fn classify_block_agent(
    block_id: &str,
    window_id: Option<&str>,
    proc_json: Option<&Value>,
    status_json: Option<&Value>,
) -> Option<RexAgentChoice> {
    let mut detected: Option<(&'static str, &'static str)> = None;
    let mut state_str = "ready".to_string();
    let mut blocked_kind: Option<String> = None;

    if let Some(st) = status_json
        && let Some(records) = st.get("records").and_then(Value::as_array)
    {
        for rec in records {
            let app = rec
                .get("effective_app")
                .or_else(|| rec.get("app"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if detected.is_none()
                && let Some(id) = match_agent_identity(app)
            {
                detected = Some(id);
            }
            if detected.is_none()
                && let Some(owner_name) = rec.pointer("/owner/name").and_then(Value::as_str)
                && let Some(id) = match_agent_identity(owner_name)
            {
                detected = Some(id);
            }
            if let Some(s) = rec.get("state").and_then(Value::as_str) {
                match s {
                    "blocked" => {
                        state_str = "blocked".to_string();
                        blocked_kind = rec
                            .get("kind")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    "working" if state_str != "blocked" => {
                        state_str = "working".to_string();
                    }
                    "idle" | "done" if state_str != "blocked" && state_str != "working" => {
                        state_str = "ready".to_string();
                    }
                    _ => {}
                }
            }
        }
    }

    let mut cwd: Option<String> = None;
    if let Some(proc) = proc_json {
        if let Some(fg) = proc.get("foreground") {
            if cwd.is_none() {
                cwd = fg
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            for field in ["name", "argv0", "invoked_path", "path"] {
                if detected.is_none()
                    && let Some(val) = fg.get(field).and_then(Value::as_str)
                    && let Some(id) = match_agent_identity(val)
                {
                    detected = Some(id);
                }
            }
        }
        if let Some(ancs) = proc.get("ancestors").and_then(Value::as_array) {
            for anc in ancs {
                if cwd.is_none() {
                    cwd = anc
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
                for field in ["name", "argv0", "invoked_path", "path"] {
                    if detected.is_none()
                        && let Some(val) = anc.get(field).and_then(Value::as_str)
                        && let Some(id) = match_agent_identity(val)
                    {
                        detected = Some(id);
                    }
                }
            }
        }
    }

    let (prog_key, prog_name) = detected?;
    let short_id = block_id
        .rsplit('-')
        .next()
        .unwrap_or(block_id)
        .chars()
        .take(6)
        .collect::<String>();

    Some(RexAgentChoice {
        id: block_id.to_string(),
        kind: "live".to_string(),
        block_id: Some(block_id.to_string()),
        window_id: window_id.map(str::to_string),
        program_key: prog_key.to_string(),
        program_name: prog_name.to_string(),
        label: format!("{prog_name} · {short_id}"),
        compact_label: format!("{prog_name}·{short_id}"),
        status: state_str,
        blocked_kind,
        cwd,
    })
}

fn find_installed_agent_launchers() -> Vec<RexAgentChoice> {
    let mut out = Vec::new();
    let path_var = env::var("PATH").unwrap_or_default();
    let home = env::var("HOME").unwrap_or_default();
    let mut search_dirs: Vec<PathBuf> = env::split_paths(&path_var).collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        search_dirs.push(PathBuf::from(extra));
    }
    if !home.is_empty() {
        search_dirs.push(PathBuf::from(&home).join(".local/bin"));
        search_dirs.push(PathBuf::from(&home).join(".cargo/bin"));
    }

    for &(key, display, aliases) in KNOWN_AGENTS {
        let bin_name = aliases[0];
        let installed = search_dirs.iter().any(|d| d.join(bin_name).is_file());
        if installed {
            out.push(RexAgentChoice {
                id: format!("__new__:{bin_name}"),
                kind: "new".to_string(),
                block_id: None,
                window_id: None,
                program_key: key.to_string(),
                program_name: display.to_string(),
                label: format!("+ New {display}"),
                compact_label: format!("+ {display}"),
                status: "new".to_string(),
                blocked_kind: None,
                cwd: None,
            });
        }
    }

    if out.is_empty() {
        out.push(RexAgentChoice {
            id: "__new__:claude".to_string(),
            kind: "new".to_string(),
            block_id: None,
            window_id: None,
            program_key: "claude".to_string(),
            program_name: "Claude Code".to_string(),
            label: "+ New Agent Split".to_string(),
            compact_label: "+ Agent".to_string(),
            status: "new".to_string(),
            blocked_kind: None,
            cwd: None,
        });
    }
    out
}

pub fn discover_rex_agents(
    session_id: Option<&str>,
    self_block_id: Option<&str>,
    source_block_id: Option<&str>,
    active_window_id: Option<&str>,
) -> Vec<RexAgentChoice> {
    let mut live_choices: Vec<RexAgentChoice> = Vec::new();

    if let Some(sid) = session_id
        && let Some(inspect) = run_rex_json(&["session", "inspect", sid, "--json"])
        && let Some(windows) = inspect.get("windows").and_then(Value::as_array)
    {
        for win in windows {
            let win_id = win.get("window_id").and_then(Value::as_str);
            let Some(layers) = win.get("layers").and_then(Value::as_array) else {
                continue;
            };
            for layer in layers {
                let Some(blocks) = layer.get("blocks").and_then(Value::as_array) else {
                    continue;
                };
                for blk in blocks {
                    let lbl = blk.get("label").and_then(Value::as_str).unwrap_or("");
                    if lbl == "omnidiff-float" || lbl == "omnidiff-dock" {
                        continue;
                    }
                    let Some(bid) = blk.get("block_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if Some(bid) == self_block_id {
                        continue;
                    }
                    let proc_json = run_rex_json(&[
                        "block", "call", "-s", sid, "-b", bid, "process", "{}",
                    ]);
                    let status_json = run_rex_json(&[
                        "block",
                        "call",
                        "-s",
                        sid,
                        "-b",
                        bid,
                        "program_status",
                        "{}",
                    ]);
                    if let Some(choice) = classify_block_agent(
                        bid,
                        win_id,
                        proc_json.as_ref(),
                        status_json.as_ref(),
                    ) {
                        live_choices.push(choice);
                    }
                }
            }
        }
    }

    let score = |c: &RexAgentChoice| -> i32 {
        let is_src = source_block_id.is_some_and(|s| c.block_id.as_deref() == Some(s));
        let same_win =
            active_window_id.is_some_and(|w| c.window_id.as_deref() == Some(w));
        let ready = c.status == "ready";
        let working = c.status == "working";
        if is_src && ready {
            80
        } else if same_win && ready {
            70
        } else if ready {
            60
        } else if is_src && working {
            50
        } else if same_win && working {
            40
        } else if working {
            30
        } else {
            10
        }
    };

    live_choices.sort_by_key(|b| std::cmp::Reverse(score(b)));
    live_choices.extend(find_installed_agent_launchers());
    live_choices
}

pub fn send_prompt_to_rex_agent(
    session_id: Option<&str>,
    source_block_id: Option<&str>,
    choice: &RexAgentChoice,
    repo_root: &Path,
    prompt: &str,
) -> Result<String> {
    let sid = session_id.context("No active REX_SESSION found")?;
    let rex_bin = find_rex_bin();

    if choice.kind == "live" {
        if choice.status == "blocked" {
            let reason = choice.blocked_kind.as_deref().unwrap_or("permission/input");
            bail!(
                "{} is blocked on {} — resolve it in the terminal or click the agent pill to switch",
                choice.label,
                reason
            );
        }
        let target_block = choice
            .block_id
            .as_deref()
            .context("live agent choice missing block_id")?;

        let send_out = Command::new(&rex_bin)
            .args(["send", "-s", sid, "-b", target_block, "--", prompt])
            .output()
            .context("failed to run rex send")?;
        if !send_out.status.success() {
            let stderr = String::from_utf8_lossy(&send_out.stderr);
            bail!("rex send failed: {}", stderr.trim());
        }

        thread::sleep(Duration::from_millis(35));
        let key_out = Command::new(&rex_bin)
            .args(["send-key", "-s", sid, "-b", target_block, "Enter"])
            .output()
            .context("failed to run rex send-key Enter")?;
        if !key_out.status.success() {
            let stderr = String::from_utf8_lossy(&key_out.stderr);
            bail!("rex send-key Enter failed: {}", stderr.trim());
        }

        return Ok(target_block.to_string());
    }

    // Spawn a new split running the selected coding agent
    let bin_name = choice
        .id
        .strip_prefix("__new__:")
        .unwrap_or("claude");
    let root_str = repo_root.to_string_lossy().into_owned();

    let mut split_cmd = Command::new(&rex_bin);
    split_cmd.args(["split", "-s", sid]);
    if let Some(src) = source_block_id.filter(|s| !s.is_empty()) {
        split_cmd.args(["-b", src]);
    }
    split_cmd.args([
        "--split=right",
        "-c",
        &root_str,
        "--focus=false",
        "--json",
        "--",
        bin_name,
    ]);

    let out = split_cmd.output().context("failed to spawn new agent split via rex split")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("rex split failed: {}", stderr.trim());
    }

    let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let new_block = parsed
        .pointer("/block_ids/0")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if !new_block.is_empty() {
        thread::sleep(Duration::from_millis(650));
        let _ = Command::new(&rex_bin)
            .args(["send", "-s", sid, "-b", &new_block, "--", prompt])
            .output();
        thread::sleep(Duration::from_millis(50));
        let _ = Command::new(&rex_bin)
            .args(["send-key", "-s", sid, "-b", &new_block, "Enter"])
            .output();
    }

    Ok(new_block)
}

pub fn close_self_in_rex(
    session_id: Option<&str>,
    self_block_id: Option<&str>,
    mode: &str,
) {
    let Some(sid) = session_id else { return };
    let Some(bid) = self_block_id else { return };
    let rex_bin = find_rex_bin();

    if mode == "float"
        && let Some(inspect) = run_rex_json(&["session", "inspect", sid, "--json"])
        && let Some(windows) = inspect.get("windows").and_then(Value::as_array)
    {
        for win in windows {
            if let Some(layers) = win.get("layers").and_then(Value::as_array) {
                for layer in layers {
                    if layer.get("kind").and_then(Value::as_str) == Some("tiled") {
                        continue;
                    }
                    let has_self = layer
                        .get("blocks")
                        .and_then(Value::as_array)
                        .is_some_and(|blks| {
                            blks.iter()
                                .any(|b| b.get("block_id").and_then(Value::as_str) == Some(bid))
                        });
                    if has_self
                        && let Some(lid) = layer.get("layer_id").and_then(Value::as_str)
                    {
                        let arg = serde_json::json!({ "layer_id": lid }).to_string();
                        let _ = Command::new(&rex_bin)
                            .args(["api", "call", "-s", sid, "session.close_layer", &arg])
                            .output();
                        return;
                    }
                }
            }
        }
    }

    let _ = Command::new(&rex_bin)
        .args(["close", "-s", sid, bid])
        .output();
}
