use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, TextDiff};

use crate::diff_engine::{map_hunk_to_index, split_lines_preserve};
use crate::git::{git_blob, git_read, git_write, has_head, hex_sha256, repo_root, short_sha256};
use crate::types::{TuiLaunchSpec, UndoSummary};

const MAX_UNDO_ENTRIES: usize = 25;

// ---- Reviewed State Persistence ---------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewedStore {
    #[serde(default)]
    pub chunks: HashSet<String>,
    #[serde(default)]
    pub files: HashMap<String, String>, // key -> short_sha256 or "DELETED"
}

pub fn make_chunk_review_key(root: &str, path: &str, sig: &str) -> String {
    format!("{root}\0{path}\0{sig}")
}

pub fn make_file_review_key(root: &str, path: &str) -> String {
    format!("{root}\0{path}")
}

fn reviewed_file_path(app_home: &Path) -> std::path::PathBuf {
    app_home.join("data").join("omnidiff").join("reviewed.json")
}

pub fn load_reviewed_map(app_home: &Path) -> ReviewedStore {
    let p = reviewed_file_path(app_home);
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_reviewed_map(app_home: &Path, store: &ReviewedStore) -> Result<()> {
    let p = reviewed_file_path(app_home);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(store)?)?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn toggle_review(
    cwd: &Path,
    app_home: &Path,
    path: &str,
    chunk_sig: Option<&str>,
    file_sig: Option<&str>,
) -> Result<bool> {
    let root = repo_root(cwd)?;
    let root_str = root.to_string_lossy().into_owned();
    let mut store = load_reviewed_map(app_home);

    let now_reviewed = if let Some(sig) = chunk_sig.filter(|s| !s.is_empty()) {
        let key = make_chunk_review_key(&root_str, path, sig);
        if store.chunks.contains(&key) {
            store.chunks.remove(&key);
            false
        } else {
            if store.chunks.len() > 5000 {
                store.chunks.clear();
            }
            store.chunks.insert(key);
            true
        }
    } else {
        let key = make_file_review_key(&root_str, path);
        let full = root.join(path);
        let current_marker = match fs::read(&full) {
            Ok(bytes) => short_sha256(&bytes),
            Err(_) => file_sig.unwrap_or("DELETED").to_string(),
        };
        if store.files.get(&key).is_some_and(|v| v == &current_marker) {
            store.files.remove(&key);
            false
        } else {
            store.files.insert(key, current_marker);
            true
        }
    };

    save_reviewed_map(app_home, &store)?;
    Ok(now_reviewed)
}

// ---- Undo Journal -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UndoRecord {
    id: String,
    repo_root: String,
    path: String,
    label: String,
    existed: bool,
    blob_file: Option<String>,
    #[serde(default)]
    mode: Option<u32>,
    created_at: u64,
}

fn undo_dir(app_home: &Path) -> std::path::PathBuf {
    app_home.join("data").join("omnidiff").join("undo")
}

fn load_undo_records(app_home: &Path) -> Vec<UndoRecord> {
    let idx = undo_dir(app_home).join("index.json");
    fs::read_to_string(&idx)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_undo_records(app_home: &Path, records: &[UndoRecord]) -> Result<()> {
    let dir = undo_dir(app_home);
    fs::create_dir_all(&dir)?;
    let idx = dir.join("index.json");
    let tmp = dir.join("index.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    fs::rename(&tmp, &idx)?;
    Ok(())
}

pub fn load_undo_summary(app_home: &Path, root_str: &str) -> (Option<UndoSummary>, usize) {
    let records = load_undo_records(app_home);
    let matching: Vec<&UndoRecord> = records.iter().filter(|r| r.repo_root == root_str).collect();
    let count = matching.len();
    let top = matching.last().map(|r| UndoSummary {
        id: r.id.clone(),
        label: r.label.clone(),
        path: r.path.clone(),
        created_at: r.created_at,
    });
    (top, count)
}

fn push_undo_snapshot(app_home: &Path, root: &Path, path: &str, label: String) -> Result<()> {
    let dir = undo_dir(app_home);
    fs::create_dir_all(&dir)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let full = root.join(path);
    let existed = full.is_file();
    let mut blob_file = None;
    let mut mode = None;

    if existed {
        let bytes = fs::read(&full)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(&full) {
                mode = Some(meta.permissions().mode());
            }
        }
        let id_hash = short_sha256(&bytes);
        let fname = format!("{now_ms}-{id_hash}.bin");
        fs::write(dir.join(&fname), &bytes)?;
        blob_file = Some(fname);
    }

    let mut records = load_undo_records(app_home);
    let id = format!("u-{now_ms}-{}", records.len());
    records.push(UndoRecord {
        id,
        repo_root: root.to_string_lossy().into_owned(),
        path: path.to_string(),
        label,
        existed,
        blob_file,
        mode,
        created_at: now_ms,
    });

    while records.len() > MAX_UNDO_ENTRIES {
        let dropped = records.remove(0);
        if let Some(bf) = dropped.blob_file {
            let _ = fs::remove_file(dir.join(bf));
        }
    }
    save_undo_records(app_home, &records)?;
    Ok(())
}

pub fn pop_undo(cwd: &Path, app_home: &Path) -> Result<UndoSummary> {
    let root = repo_root(cwd)?;
    let root_str = root.to_string_lossy().into_owned();
    let dir = undo_dir(app_home);
    let mut records = load_undo_records(app_home);
    let Some(pos) = records.iter().rposition(|r| r.repo_root == root_str) else {
        bail!("Nothing to undo for this repository");
    };
    let rec = records.remove(pos);
    let target = root.join(&rec.path);

    if rec.existed {
        let bf = rec
            .blob_file
            .as_ref()
            .context("undo record missing snapshot")?;
        let bytes = fs::read(dir.join(bf)).context("undo snapshot file missing")?;
        atomic_write_file(&target, &bytes, rec.mode)?;
        let _ = fs::remove_file(dir.join(bf));
    } else if target.is_file() {
        fs::remove_file(&target)?;
    }

    save_undo_records(app_home, &records)?;
    Ok(UndoSummary {
        id: rec.id,
        label: rec.label,
        path: rec.path,
        created_at: rec.created_at,
    })
}

fn atomic_write_file(target: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing_mode = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(target).ok().map(|m| m.permissions().mode())
        }
        #[cfg(not(unix))]
        {
            None::<u32>
        }
    };
    let tmp = target.with_extension("omnidiff-tmp");
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    if let Some(m) = mode.or(existing_mode) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(m));
    }
    #[cfg(not(unix))]
    let _ = (mode, existing_mode);
    fs::rename(&tmp, target)?;
    Ok(())
}

// ---- Line-Range Splicing & Unified Patch Application ------------------------

pub fn splice_line_range(
    dest_text: &str,
    dest_start: usize,
    dest_end: usize,
    src_text: &str,
    src_start: usize,
    src_end: usize,
) -> Result<String> {
    let (dest_lines, dest_trailing_nl) = split_lines_preserve(dest_text);
    let (src_lines, src_trailing_nl) = split_lines_preserve(src_text);

    if dest_start > dest_end || dest_end > dest_lines.len() {
        bail!(
            "destination range {}..{} out of bounds (len {})",
            dest_start,
            dest_end,
            dest_lines.len()
        );
    }
    if src_start > src_end || src_end > src_lines.len() {
        bail!(
            "source range {}..{} out of bounds (len {})",
            src_start,
            src_end,
            src_lines.len()
        );
    }

    let line_ending =
        if dest_text.contains("\r\n") || (dest_text.is_empty() && src_text.contains("\r\n")) {
            "\r\n"
        } else {
            "\n"
        };

    let mut out_lines: Vec<&str> =
        Vec::with_capacity(dest_lines.len() - (dest_end - dest_start) + (src_end - src_start));
    out_lines.extend_from_slice(&dest_lines[..dest_start]);
    out_lines.extend_from_slice(&src_lines[src_start..src_end]);
    out_lines.extend_from_slice(&dest_lines[dest_end..]);

    if out_lines.is_empty() {
        return Ok(String::new());
    }

    let keep_trailing_nl = if dest_lines.is_empty()
        || (dest_end == dest_lines.len() && src_end > src_start && src_end == src_lines.len())
    {
        src_trailing_nl
    } else {
        dest_trailing_nl
    };

    let mut result = out_lines.join(line_ending);
    if keep_trailing_nl {
        result.push_str(line_ending);
    }
    Ok(result)
}

static MERGE_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempMergeDir(PathBuf);

impl Drop for TempMergeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn merge_three_texts(root: &Path, ours: &str, base: &str, theirs: &str) -> Result<String> {
    let seq = MERGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = TempMergeDir(std::env::temp_dir().join(format!(
        "rex-omnidiff-merge-{}-{now_ns}-{seq}",
        std::process::id()
    )));
    fs::create_dir_all(&tmp.0)?;
    let ours_p = tmp.0.join("ours");
    let base_p = tmp.0.join("base");
    let theirs_p = tmp.0.join("theirs");
    fs::write(&ours_p, ours.as_bytes())?;
    fs::write(&base_p, base.as_bytes())?;
    fs::write(&theirs_p, theirs.as_bytes())?;

    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("merge-file")
        .arg("--stdout")
        .arg("--theirs")
        .arg(&ours_p)
        .arg(&base_p)
        .arg(&theirs_p)
        .output()
        .context("failed to spawn git merge-file")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git merge-file failed: {}", stderr.trim());
    }
    String::from_utf8(out.stdout).context("merged index content is not valid UTF-8")
}

fn parse_git_mode(raw: &[u8]) -> Option<String> {
    let tok = String::from_utf8_lossy(raw)
        .split_whitespace()
        .next()
        .map(str::to_string)?;
    if matches!(tok.as_str(), "100644" | "100755") {
        Some(tok)
    } else {
        None
    }
}

fn detect_stage_mode(root: &Path, path: &str, prev_path: &str, full: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(full)
            && meta.is_file()
        {
            return if (meta.permissions().mode() & 0o111) != 0 {
                "100755".to_string()
            } else {
                "100644".to_string()
            };
        }
    }
    let _ = full;
    if let Ok(out) = git_read(root, &["ls-files", "-s", "--", path])
        && let Some(m) = parse_git_mode(&out)
    {
        return m;
    }
    if has_head(root)
        && let Ok(out) = git_read(root, &["ls-tree", "HEAD", "--", prev_path])
        && let Some(m) = parse_git_mode(&out)
    {
        return m;
    }
    "100644".to_string()
}

fn detect_unstage_mode(root: &Path, path: &str, prev_path: &str) -> String {
    if has_head(root)
        && let Ok(out) = git_read(root, &["ls-tree", "HEAD", "--", prev_path])
        && let Some(m) = parse_git_mode(&out)
    {
        return m;
    }
    if let Ok(out) = git_read(root, &["ls-files", "-s", "--", path])
        && let Some(m) = parse_git_mode(&out)
    {
        return m;
    }
    "100644".to_string()
}

fn write_index_blob(root: &Path, path: &str, mode: &str, bytes: &[u8]) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("hash-object")
        .arg("-w")
        .arg(format!("--path={path}"))
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git hash-object")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(bytes)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git hash-object failed: {}", stderr.trim());
    }
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if oid.is_empty() {
        bail!("git hash-object returned empty object ID");
    }
    git_write(
        root,
        &["update-index", "--add", "--cacheinfo", mode, &oid, path],
    )?;
    let _ = git_write(root, &["update-index", "-q", "--refresh"]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_chunk_sig(
    path: &str,
    before_text: &str,
    after_text: &str,
    b_start: usize,
    b_end: usize,
    a_start: usize,
    a_end: usize,
    expected_sig: Option<&str>,
) -> Result<()> {
    let Some(exp) = expected_sig.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let (before_lines, _) = split_lines_preserve(before_text);
    let (after_lines, _) = split_lines_preserve(after_text);
    if b_end > before_lines.len() || a_end > after_lines.len() {
        bail!("File changed since diff was loaded; refresh and try again");
    }
    let mut sig_input = format!("{path}:{}:{}\n", b_end - b_start, a_end - a_start);
    for l in &before_lines[b_start..b_end] {
        sig_input.push('-');
        sig_input.push_str(l);
        sig_input.push('\n');
    }
    for l in &after_lines[a_start..a_end] {
        sig_input.push('+');
        sig_input.push_str(l);
        sig_input.push('\n');
    }
    let actual = short_sha256(sig_input.as_bytes());
    if actual != exp {
        bail!("Hunk changed on disk since diff was loaded; refreshed view required");
    }
    Ok(())
}

// ---- Public File & Chunk Operations -----------------------------------------

pub fn stage_file(cwd: &Path, path: &str, old_path: Option<&str>) -> Result<()> {
    let root = repo_root(cwd)?;
    if let Some(old) = old_path.filter(|o| !o.is_empty() && *o != path) {
        git_write(&root, &["add", "--", old, path])?;
    } else {
        git_write(&root, &["add", "--", path])?;
    }
    Ok(())
}

pub fn unstage_file(cwd: &Path, path: &str, old_path: Option<&str>) -> Result<()> {
    let root = repo_root(cwd)?;
    if let Some(old) = old_path.filter(|o| !o.is_empty() && *o != path) {
        if has_head(&root) {
            git_write(&root, &["restore", "--staged", "--", old, path])?;
        } else {
            git_write(&root, &["rm", "--cached", "-f", "--", old, path])?;
        }
    } else if has_head(&root) {
        git_write(&root, &["restore", "--staged", "--", path])?;
    } else {
        git_write(&root, &["rm", "--cached", "-f", "--", path])?;
    }
    Ok(())
}

pub fn discard_file(cwd: &Path, app_home: &Path, scope: &str, path: &str) -> Result<()> {
    let root = repo_root(cwd)?;
    push_undo_snapshot(app_home, &root, path, format!("Discard file {path}"))?;

    let tracked = git_read(&root, &["ls-files", "--error-unmatch", "--", path]).is_ok();
    if !tracked {
        let full = root.join(path);
        if full.is_file() {
            fs::remove_file(&full)?;
        }
        return Ok(());
    }

    if scope == "all" && has_head(&root) {
        git_write(&root, &["checkout", "HEAD", "--", path])?;
    } else {
        git_write(&root, &["checkout", "--", path])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn stage_chunk(
    cwd: &Path,
    scope: &str,
    path: &str,
    old_path: Option<&str>,
    b_start: usize,
    b_end: usize,
    a_start: usize,
    a_end: usize,
    expected_sig: Option<&str>,
) -> Result<()> {
    let root = repo_root(cwd)?;
    let full = root.join(path);
    let prev = old_path.filter(|s| !s.is_empty()).unwrap_or(path);
    let wt_exists = fs::symlink_metadata(&full).is_ok();
    let after_bytes = if full.is_file() {
        fs::read(&full)?
    } else {
        Vec::new()
    };
    let after_text = String::from_utf8(after_bytes).context("cannot stage hunk in binary file")?;

    let new_index_text = if scope == "all" {
        let head_bytes = if has_head(&root) {
            git_blob(&root, "HEAD", prev).unwrap_or_default()
        } else {
            Vec::new()
        };
        let head_text =
            String::from_utf8(head_bytes).context("cannot stage hunk in binary file")?;

        verify_chunk_sig(
            path,
            &head_text,
            &after_text,
            b_start,
            b_end,
            a_start,
            a_end,
            expected_sig,
        )?;

        let index_opt = git_blob(&root, "", path);
        if index_opt.as_deref() == Some(head_text.as_bytes())
            || (index_opt.is_none() && head_text.is_empty())
        {
            splice_line_range(&head_text, b_start, b_end, &after_text, a_start, a_end)?
        } else {
            let index_bytes = index_opt.unwrap_or_default();
            let index_text =
                String::from_utf8(index_bytes).context("cannot stage hunk in binary file")?;
            let (h_lines, _) = split_lines_preserve(&head_text);
            let (i_lines, _) = split_lines_preserve(&index_text);
            let td = TextDiff::configure()
                .algorithm(Algorithm::Patience)
                .diff_slices(&h_lines, &i_lines);
            if let Some((i_start, i_end)) = map_hunk_to_index(td.ops(), b_start, b_end) {
                splice_line_range(&index_text, i_start, i_end, &after_text, a_start, a_end)?
            } else {
                let target_from_head =
                    splice_line_range(&head_text, b_start, b_end, &after_text, a_start, a_end)?;
                merge_three_texts(&root, &index_text, &head_text, &target_from_head)?
            }
        }
    } else {
        let index_bytes = git_blob(&root, "", prev).unwrap_or_default();
        let index_text =
            String::from_utf8(index_bytes).context("cannot stage hunk in binary file")?;

        verify_chunk_sig(
            path,
            &index_text,
            &after_text,
            b_start,
            b_end,
            a_start,
            a_end,
            expected_sig,
        )?;

        splice_line_range(&index_text, b_start, b_end, &after_text, a_start, a_end)?
    };

    if !wt_exists && new_index_text.is_empty() {
        git_write(&root, &["update-index", "--force-remove", "--", path])?;
        return Ok(());
    }

    let mode = detect_stage_mode(&root, path, prev, &full);
    write_index_blob(&root, path, &mode, new_index_text.as_bytes())
}

#[allow(clippy::too_many_arguments)]
pub fn unstage_chunk(
    cwd: &Path,
    scope: &str,
    path: &str,
    old_path: Option<&str>,
    b_start: usize,
    b_end: usize,
    a_start: usize,
    a_end: usize,
    expected_sig: Option<&str>,
) -> Result<()> {
    let root = repo_root(cwd)?;
    let full = root.join(path);
    let prev = old_path.filter(|s| !s.is_empty()).unwrap_or(path);
    let head_opt = if has_head(&root) {
        git_blob(&root, "HEAD", prev)
    } else {
        None
    };
    let existed_in_head = head_opt.is_some();
    let head_bytes = head_opt.unwrap_or_default();
    let head_text = String::from_utf8(head_bytes).context("cannot unstage hunk in binary file")?;

    let new_index_text = if scope == "all" {
        let after_bytes = if full.is_file() {
            fs::read(&full)?
        } else {
            Vec::new()
        };
        let after_text =
            String::from_utf8(after_bytes).context("cannot unstage hunk in binary file")?;

        verify_chunk_sig(
            path,
            &head_text,
            &after_text,
            b_start,
            b_end,
            a_start,
            a_end,
            expected_sig,
        )?;

        let index_opt = git_blob(&root, "", path);
        if !existed_in_head && index_opt.is_none() {
            return Ok(());
        }
        let index_bytes = index_opt.unwrap_or_default();
        if existed_in_head && index_bytes == head_text.as_bytes() {
            return Ok(());
        }
        let index_text =
            String::from_utf8(index_bytes).context("cannot unstage hunk in binary file")?;
        let (h_lines, _) = split_lines_preserve(&head_text);
        let (i_lines, _) = split_lines_preserve(&index_text);
        let td = TextDiff::configure()
            .algorithm(Algorithm::Patience)
            .diff_slices(&h_lines, &i_lines);
        if let Some((i_start, i_end)) = map_hunk_to_index(td.ops(), b_start, b_end) {
            splice_line_range(&index_text, i_start, i_end, &head_text, b_start, b_end)?
        } else {
            let head_with_chunk =
                splice_line_range(&head_text, b_start, b_end, &after_text, a_start, a_end)?;
            merge_three_texts(&root, &index_text, &head_with_chunk, &head_text)?
        }
    } else {
        let index_bytes = git_blob(&root, "", path).unwrap_or_default();
        let index_text =
            String::from_utf8(index_bytes).context("cannot unstage hunk in binary file")?;

        verify_chunk_sig(
            path,
            &head_text,
            &index_text,
            b_start,
            b_end,
            a_start,
            a_end,
            expected_sig,
        )?;

        splice_line_range(&index_text, a_start, a_end, &head_text, b_start, b_end)?
    };

    if !existed_in_head && new_index_text.is_empty() {
        git_write(&root, &["update-index", "--force-remove", "--", path])?;
        return Ok(());
    }

    let mode = detect_unstage_mode(&root, path, prev);
    write_index_blob(&root, path, &mode, new_index_text.as_bytes())
}

#[allow(clippy::too_many_arguments)]
pub fn revert_chunk(
    cwd: &Path,
    app_home: &Path,
    scope: &str,
    path: &str,
    old_path: Option<&str>,
    b_start: usize,
    b_end: usize,
    a_start: usize,
    a_end: usize,
    expected_hash: Option<&str>,
    expected_sig: Option<&str>,
) -> Result<()> {
    let root = repo_root(cwd)?;
    let full = root.join(path);
    let prev = old_path.unwrap_or(path);

    let after_bytes = if full.is_file() {
        fs::read(&full)?
    } else {
        Vec::new()
    };

    if let Some(exp_hash) = expected_hash.filter(|s| !s.is_empty()) {
        let actual_hash = hex_sha256(&after_bytes);
        if actual_hash != exp_hash {
            bail!(
                "File {} was modified concurrently on disk; refusing to overwrite",
                path
            );
        }
    }

    let before_bytes = if scope == "unstaged" {
        git_blob(&root, "", prev).unwrap_or_default()
    } else if has_head(&root) {
        git_blob(&root, "HEAD", prev).unwrap_or_default()
    } else {
        Vec::new()
    };

    let before_text = String::from_utf8(before_bytes).context("cannot revert hunk in binary file")?;
    let after_text = String::from_utf8(after_bytes).context("cannot revert hunk in binary file")?;

    verify_chunk_sig(
        path,
        &before_text,
        &after_text,
        b_start,
        b_end,
        a_start,
        a_end,
        expected_sig,
    )?;

    let reverted_text = splice_line_range(&after_text, a_start, a_end, &before_text, b_start, b_end)?;

    push_undo_snapshot(
        app_home,
        &root,
        path,
        format!("Revert hunk in {path} (L{})", a_start + 1),
    )?;

    if reverted_text.is_empty() && before_text.is_empty() {
        let tracked = git_read(&root, &["ls-files", "--error-unmatch", "--", path]).is_ok();
        if !tracked && full.is_file() {
            fs::remove_file(&full)?;
            return Ok(());
        }
    }

    atomic_write_file(&full, reverted_text.as_bytes(), None)?;
    let _ = git_write(&root, &["update-index", "-q", "--refresh"]);
    Ok(())
}

pub fn commit_changes(cwd: &Path, message: &str, stage_all: bool) -> Result<()> {
    let msg = message.trim();
    if msg.is_empty() {
        bail!("Commit message cannot be empty");
    }
    let root = repo_root(cwd)?;
    if stage_all {
        git_write(&root, &["add", "-A"])?;
    }
    git_write(&root, &["commit", "-m", msg])?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiCustomPalette {
    pub insert_bg: String,
    pub delete_bg: String,
    pub move_bg: String,
    pub update_bg: String,
    pub overlay_fg: String,
    pub cross_highlight_bg: String,
    pub search_bg: String,
    pub before_title_fg: String,
    pub after_title_fg: String,
}

fn valid_hex6(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn write_tui_theme_config(app_home: &Path, palette_json: &str) -> Option<PathBuf> {
    let pal: TuiCustomPalette = serde_json::from_str(palette_json).ok()?;
    let fields = [
        &pal.insert_bg,
        &pal.delete_bg,
        &pal.move_bg,
        &pal.update_bg,
        &pal.overlay_fg,
        &pal.cross_highlight_bg,
        &pal.search_bg,
        &pal.before_title_fg,
        &pal.after_title_fg,
    ];
    if !fields.iter().all(|c| valid_hex6(c)) {
        return None;
    }

    let dir = app_home.join("data").join("omnidiff");
    fs::create_dir_all(&dir).ok()?;
    let cfg_path = dir.join("tui-config.toml");
    let toml = format!(
        "theme = \"Custom\"\n\n\
         [custom_palette]\n\
         insert_bg = \"{}\"\n\
         delete_bg = \"{}\"\n\
         move_bg = \"{}\"\n\
         update_bg = \"{}\"\n\
         overlay_fg = \"{}\"\n\
         cross_highlight_bg = \"{}\"\n\
         search_bg = \"{}\"\n\
         before_title_fg = \"{}\"\n\
         after_title_fg = \"{}\"\n",
        pal.insert_bg,
        pal.delete_bg,
        pal.move_bg,
        pal.update_bg,
        pal.overlay_fg,
        pal.cross_highlight_bg,
        pal.search_bg,
        pal.before_title_fg,
        pal.after_title_fg,
    );
    let tmp = dir.join(".tui-config.toml.tmp");
    fs::write(&tmp, toml).ok()?;
    fs::rename(&tmp, &cfg_path).ok()?;
    Some(cfg_path)
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_tui_launch(
    cwd: &Path,
    app_home: &Path,
    scope: &str,
    path: Option<&str>,
    old_path: Option<&str>,
    commit_hash: Option<&str>,
    preset: &str,
    palette_json: Option<&str>,
) -> Result<TuiLaunchSpec> {
    let root = repo_root(cwd)?;
    let root_str = root.to_string_lossy().into_owned();
    let bin = crate::git::find_omnidiff_bin().unwrap_or_else(|| "omnidiff".to_string());

    let mut argv = Vec::new();
    #[cfg(unix)]
    if let Some(cfg_path) = palette_json.and_then(|j| write_tui_theme_config(app_home, j)) {
        argv.push("/usr/bin/env".to_string());
        argv.push(format!("OMNIDIFF_CONFIG={}", cfg_path.display()));
    }
    argv.push(bin);
    match preset {
        "minimal" => argv.push("--minimal".to_string()),
        "full" => argv.push("--full".to_string()),
        _ => {}
    }

    if let Some(p) = path.filter(|s| !s.is_empty()) {
        let mat = crate::git::materialize_pair(
            &root,
            app_home,
            "tui-pair",
            scope,
            p,
            old_path,
            commit_hash,
        )?;
        argv.push(mat.before_path.to_string_lossy().into_owned());
        argv.push(mat.after_path.to_string_lossy().into_owned());
        Ok(TuiLaunchSpec {
            argv,
            cwd: root_str,
            title: format!("OmniDiff · {p}"),
        })
    } else {
        argv.push("--review".to_string());
        Ok(TuiLaunchSpec {
            argv,
            cwd: root_str,
            title: "OmniDiff Review".to_string(),
        })
    }
}

pub fn cleanup_scratch(app_home: &Path) -> Result<()> {
    let scratch = app_home.join("data").join("omnidiff").join("scratch");
    if scratch.exists() {
        let _ = fs::remove_dir_all(&scratch);
    }
    Ok(())
}
