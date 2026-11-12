use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use similar::{Algorithm, DiffOp, TextDiff};

use crate::actions::{load_reviewed_map, make_chunk_review_key, make_file_review_key};
use crate::git::{find_omnidiff_bin, materialize_pair, repo_root, short_sha256};
use crate::types::{
    ChangeCounts, ChunkInfo, DiffResponse, FileEntry, MoveInfo, OmniDiffJson, OmniHunk, SplitCell,
    SplitRow, TextSegment, UnifiedRow,
};

const TAB_WIDTH: usize = 4;
const MAX_LINE_CHARS: usize = 2000;
const MAX_RENDER_ROWS: usize = 12_000;
const MAX_OMNIDIFF_BYTES: usize = 4 * 1024 * 1024;
const MAX_SNIPPET_LINES: usize = 80;

#[derive(Debug, Clone, Default)]
struct RowAnnotations {
    has_insert: bool,
    has_delete: bool,
    has_update: bool,
    has_move: bool,
    spans: Vec<(usize, usize, &'static str)>,
    ref_line: Option<usize>,
    move_dest_min: Option<usize>,
    move_dest_max: Option<usize>,
}

impl RowAnnotations {
    fn any_change(&self) -> bool {
        self.has_insert || self.has_delete || self.has_update || self.has_move
    }

    fn marker(&self, is_before: bool) -> String {
        if is_before {
            "-".to_string()
        } else {
            "+".to_string()
        }
    }

    fn row_bg(&self, is_before: bool) -> Option<String> {
        if is_before {
            Some("d".to_string())
        } else {
            Some("i".to_string())
        }
    }
}

pub fn split_lines_preserve(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), true);
    }
    let trailing_nl = text.ends_with('\n');
    let mut lines: Vec<&str> = text.split('\n').collect();
    if trailing_nl {
        lines.pop();
    }
    for line in &mut lines {
        if let Some(stripped) = line.strip_suffix('\r') {
            *line = stripped;
        }
    }
    (lines, trailing_nl)
}

/// Maps a base (`HEAD`) line range `b_start..b_end` onto the corresponding line range
/// `i_start..i_end` in the Git index using Patience `DiffOp`s between `base` and `index`.
pub fn map_hunk_to_index(ops: &[DiffOp], b_start: usize, b_end: usize) -> Option<(usize, usize)> {
    if b_start == b_end {
        let p = b_start;
        if ops.is_empty() {
            return if p == 0 { Some((0, 0)) } else { None };
        }
        for op in ops {
            match *op {
                DiffOp::Delete {
                    old_index, old_len, ..
                }
                | DiffOp::Replace {
                    old_index, old_len, ..
                } => {
                    if old_index <= p && p <= old_index + old_len {
                        return None;
                    }
                }
                _ => {}
            }
        }
        for op in ops {
            if let DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } = *op
                && old_index == p
            {
                return Some((new_index, new_index + new_len));
            }
        }
        for op in ops {
            if let DiffOp::Equal {
                old_index,
                new_index,
                len,
            } = *op
                && old_index <= p
                && p <= old_index + len
            {
                let pos = new_index + (p - old_index);
                return Some((pos, pos));
            }
        }
        return None;
    }

    let mut range: Option<(usize, usize)> = None;
    let mut extend = |s: usize, e: usize| {
        range = Some(match range {
            None => (s, e),
            Some((lo, hi)) => (lo.min(s), hi.max(e)),
        });
    };

    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                let (oh0, oh1) = (old_index, old_index + len);
                if oh1 > b_start && oh0 < b_end {
                    let cs = b_start.max(oh0);
                    let ce = b_end.min(oh1);
                    extend(new_index + (cs - oh0), new_index + (ce - oh0));
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                let (oh0, oh1) = (old_index, old_index + old_len);
                if (oh0 < b_start && b_start < oh1) || (oh0 < b_end && b_end < oh1) {
                    return None;
                }
                if oh1 > b_start && oh0 < b_end {
                    extend(new_index, new_index);
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let (oh0, oh1) = (old_index, old_index + old_len);
                if (oh0 < b_start && b_start < oh1) || (oh0 < b_end && b_end < oh1) {
                    return None;
                }
                if oh1 > b_start && oh0 < b_end {
                    extend(new_index, new_index + new_len);
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if b_start < old_index && old_index < b_end {
                    extend(new_index, new_index + new_len);
                }
            }
        }
    }

    range
}

#[derive(Debug, Clone, Copy)]
pub struct IndexSnapshot<'a> {
    pub text: Option<&'a str>,
    pub wt_exists: bool,
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len().is_multiple_of(2) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    if bytes.starts_with(&[0xFE, 0xFF]) && bytes.len().is_multiple_of(2) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn op_priority(op: &str) -> u8 {
    match op {
        "u" => 4,
        "d" => 3,
        "i" => 3,
        "m" => 1,
        _ => 0,
    }
}

/// Slices a single UTF-8 line using Tree-sitter byte offsets, expands tabs to spaces,
/// and coalesces runs with identical operation tags.
pub fn slice_line_segments(
    line: &str,
    spans: &[(usize, usize, &'static str)],
) -> (Vec<TextSegment>, usize) {
    if line.is_empty() {
        return (
            vec![TextSegment {
                t: " ".to_string(),
                op: None,
            }],
            1,
        );
    }

    let trimmed_byte_len = line.trim_end().len();
    let mut edges = vec![0usize, line.len()];
    for &(s, e, _) in spans {
        let cs = line.floor_char_boundary(s.min(trimmed_byte_len));
        let ce = line.floor_char_boundary(e.min(trimmed_byte_len));
        if cs < ce {
            edges.push(cs);
            edges.push(ce);
        }
    }
    edges.sort_unstable();
    edges.dedup();

    let mut out: Vec<TextSegment> = Vec::new();
    let mut display_col = 0usize;
    let mut truncated = false;

    for win in edges.windows(2) {
        let (start, end) = (win[0], win[1]);
        if start >= end {
            continue;
        }
        let mut chosen: Option<&'static str> = None;
        for &(s, e, op) in spans {
            let cs = line.floor_char_boundary(s.min(trimmed_byte_len));
            let ce = line.floor_char_boundary(e.min(trimmed_byte_len));
            if cs <= start
                && end <= ce
                && cs < ce
                && chosen.is_none_or(|prev| op_priority(op) >= op_priority(prev))
            {
                chosen = Some(op);
            }
        }

        let slice = &line[start..end];
        let mut expanded = String::with_capacity(slice.len());
        for ch in slice.chars() {
            if display_col >= MAX_LINE_CHARS {
                expanded.push('…');
                display_col += 1;
                truncated = true;
                break;
            }
            if ch == '\t' {
                let spaces = TAB_WIDTH - (display_col % TAB_WIDTH);
                for _ in 0..spaces {
                    expanded.push(' ');
                }
                display_col += spaces;
            } else {
                expanded.push(ch);
                display_col += 1;
            }
        }

        if !expanded.is_empty() {
            let op_str = chosen.map(str::to_string);
            if let Some(last) = out.last_mut()
                && last.op == op_str
            {
                last.t.push_str(&expanded);
            } else {
                out.push(TextSegment {
                    t: expanded,
                    op: op_str,
                });
            }
        }
        if truncated {
            break;
        }
    }

    (out, display_col.max(1))
}

fn build_row_annotations(
    lines: &[&str],
    hunks: &[OmniHunk],
    counts: &mut ChangeCounts,
    count_moves: bool,
) -> Vec<RowAnnotations> {
    let mut rows = vec![RowAnnotations::default(); lines.len()];
    for h in hunks {
        if h.range.is_empty() {
            continue;
        }
        let tag: &'static str = match h.operation.as_str() {
            "insert" => {
                counts.insertions += 1;
                "i"
            }
            "delete" => {
                counts.deletions += 1;
                "d"
            }
            "update" => {
                counts.updates += 1;
                "u"
            }
            "move" => {
                if count_moves {
                    counts.moves += 1;
                }
                "m"
            }
            _ => continue,
        };
        if lines.is_empty() || h.range.start_row >= lines.len() {
            continue;
        }
        let last_row = h.range.last_touched_row().min(lines.len() - 1);
        for r in h.range.start_row..=last_row {
            let row_len = lines[r].trim_end().len();
            let col_span = h.range.columns_on_row(r, row_len);
            let ann = &mut rows[r];
            if ann.ref_line.is_none() {
                ann.ref_line = h.reference_line;
            }
            match tag {
                "i" => ann.has_insert = true,
                "d" => ann.has_delete = true,
                "u" => ann.has_update = true,
                "m" => {
                    ann.has_move = true;
                    if let Some(dest) = &h.move_target {
                        let d_start = dest.start_row;
                        let d_end = dest.last_touched_row();
                        ann.move_dest_min =
                            Some(ann.move_dest_min.map_or(d_start, |v| v.min(d_start)));
                        ann.move_dest_max =
                            Some(ann.move_dest_max.map_or(d_end, |v| v.max(d_end)));
                    }
                }
                _ => {}
            }
            if let Some((sc, ec)) = col_span {
                ann.spans.push((sc, ec, tag));
            }
        }
    }
    rows
}

const MAX_AST_CACHE_FILES: usize = 48;
const TRIM_AST_CACHE_TO: usize = 32;
static AST_CACHE_SEQ: AtomicU64 = AtomicU64::new(0);

fn preset_cache_slug(preset: &str) -> &'static str {
    match preset {
        "minimal" => "minimal",
        "full" => "full",
        _ => "default",
    }
}

pub(crate) fn ast_cache_path(app_home: &Path, file_sig: &str, preset: &str) -> std::path::PathBuf {
    app_home
        .join("data")
        .join("omnidiff")
        .join("scratch")
        .join("ast-cache")
        .join(format!("{file_sig}-{}.json", preset_cache_slug(preset)))
}

fn prune_ast_cache(cache_dir: &Path) {
    let Ok(rd) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut entries: Vec<(SystemTime, std::path::PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let name = e.file_name();
            let s = name.to_string_lossy();
            if !s.ends_with(".json") || s.starts_with('.') {
                return None;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((mtime, p))
        })
        .collect();
    if entries.len() <= MAX_AST_CACHE_FILES {
        return;
    }
    entries.sort_unstable_by_key(|(mtime, _)| *mtime);
    let excess = entries.len() - TRIM_AST_CACHE_TO;
    for (_, p) in entries.into_iter().take(excess) {
        let _ = fs::remove_file(p);
    }
}

fn run_omnidiff_cli(
    bin: &str,
    before_path: &Path,
    after_path: &Path,
    preset: &str,
) -> Option<(OmniDiffJson, Vec<u8>)> {
    let mut cmd = Command::new(bin);
    cmd.arg("--mode").arg("json");
    match preset {
        "minimal" => {
            cmd.arg("--minimal");
        }
        "full" => {
            cmd.arg("--full");
        }
        _ => {}
    }
    cmd.arg(before_path).arg(after_path);
    let out = cmd.output().ok()?;
    if !out.status.success() && out.status.code() != Some(1) {
        return None;
    }
    let parsed = serde_json::from_slice::<OmniDiffJson>(&out.stdout).ok()?;
    Some((parsed, out.stdout))
}

fn load_or_run_omnidiff(
    app_home: &Path,
    bin: &str,
    before_path: &Path,
    after_path: &Path,
    preset: &str,
    file_sig: &str,
) -> Option<OmniDiffJson> {
    let cache_file = ast_cache_path(app_home, file_sig, preset);
    if let Ok(bytes) = fs::read(&cache_file)
        && let Ok(cached) = serde_json::from_slice::<OmniDiffJson>(&bytes)
    {
        return Some(cached);
    }

    let (parsed, raw_stdout) = run_omnidiff_cli(bin, before_path, after_path, preset)?;
    if let Some(cache_dir) = cache_file.parent()
        && fs::create_dir_all(cache_dir).is_ok()
    {
        let seq = AST_CACHE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = cache_dir.join(format!(".tmp-{}-{seq}.json", std::process::id()));
        if fs::write(&tmp, &raw_stdout).is_ok() {
            let _ = fs::rename(&tmp, &cache_file);
            prune_ast_cache(cache_dir);
        }
    }
    Some(parsed)
}

pub struct DiffParams<'a> {
    pub cwd: &'a Path,
    pub app_home: &'a Path,
    pub scope: &'a str,
    pub path: &'a str,
    pub old_path: Option<&'a str>,
    pub commit_hash: Option<&'a str>,
    pub preset: &'a str,
    pub context_lines: usize,
    pub expanded_folds: &'a HashSet<String>,
}

pub fn compute_diff(params: &DiffParams<'_>) -> Result<DiffResponse> {
    let root = repo_root(params.cwd)?;
    let root_str = root.to_string_lossy().into_owned();
    let mat = materialize_pair(
        &root,
        params.app_home,
        "scratch",
        params.scope,
        params.path,
        params.old_path,
        params.commit_hash,
    )?;

    let mut sig_buf = Vec::with_capacity(64 + params.path.len());
    sig_buf.extend_from_slice(params.path.as_bytes());
    sig_buf.push(0);
    sig_buf.extend_from_slice(short_sha256(&mat.before_bytes).as_bytes());
    sig_buf.push(b':');
    sig_buf.extend_from_slice(short_sha256(&mat.after_bytes).as_bytes());
    let file_sig = short_sha256(&sig_buf);

    let reviewed_map = load_reviewed_map(params.app_home);
    let file_review_key = make_file_review_key(&root_str, params.path);
    let current_wt_short = mat
        .worktree_sha256
        .as_deref()
        .map(|h| h[..16].to_string())
        .unwrap_or_else(|| "DELETED".to_string());
    let file_reviewed = reviewed_map
        .files
        .get(&file_review_key)
        .is_some_and(|saved| saved == &current_wt_short || saved == &file_sig);

    let within_cap =
        mat.before_bytes.len() <= MAX_OMNIDIFF_BYTES && mat.after_bytes.len() <= MAX_OMNIDIFF_BYTES;
    let omni_opt = if within_cap {
        find_omnidiff_bin().and_then(|bin| {
            load_or_run_omnidiff(
                params.app_home,
                &bin,
                &mat.before_path,
                &mat.after_path,
                params.preset,
                &file_sig,
            )
        })
    } else {
        None
    };

    let engine = if omni_opt.is_some() {
        "omnidiff".to_string()
    } else {
        "fallback".to_string()
    };

    let before_text_opt = decode_text(&mat.before_bytes);
    let after_text_opt = decode_text(&mat.after_bytes);
    let is_binary = omni_opt
        .as_ref()
        .and_then(|o| o.binary)
        .unwrap_or(false)
        || before_text_opt.is_none()
        || after_text_opt.is_none();

    if is_binary {
        let language = omni_opt
            .as_ref()
            .and_then(|o| o.after.language.clone().or_else(|| o.before.language.clone()));
        let content = omni_opt.and_then(|o| o.content.or(o.picture));
        return Ok(DiffResponse {
            path: params.path.to_string(),
            old_path: params.old_path.map(str::to_string),
            scope: params.scope.to_string(),
            language,
            encoding: None,
            summary: None,
            large_residual: false,
            engine,
            binary: true,
            content,
            picture_media: mat.picture_media,
            file_sha256: mat.worktree_sha256,
            file_sig,
            file_reviewed,
            counts: ChangeCounts::default(),
            chunks: Vec::new(),
            unified_rows: Vec::new(),
            split_rows: Vec::new(),
            max_unified_chars: 60,
            max_split_chars: 40,
            truncated: false,
        });
    }

    let before_text = before_text_opt.context("missing before text")?;
    let after_text = after_text_opt.context("missing after text")?;
    let index_text_decoded = mat.index_bytes.as_deref().and_then(decode_text);
    let index_snap = IndexSnapshot {
        text: index_text_decoded.as_deref(),
        wt_exists: mat.wt_exists,
    };

    Ok(build_text_diff_response_with_index(
        &root_str,
        params.path,
        params.old_path,
        params.scope,
        &before_text,
        &after_text,
        Some(index_snap),
        omni_opt,
        engine,
        mat.worktree_sha256,
        file_sig,
        file_reviewed,
        &reviewed_map.chunks,
        params.context_lines,
        params.expanded_folds,
    ))
}

#[allow(clippy::too_many_arguments, dead_code)]
pub fn build_text_diff_response(
    root_str: &str,
    path: &str,
    old_path: Option<&str>,
    scope: &str,
    before_text: &str,
    after_text: &str,
    omni_opt: Option<OmniDiffJson>,
    engine: String,
    file_sha256: Option<String>,
    file_sig: String,
    file_reviewed: bool,
    reviewed_chunks: &HashSet<String>,
    context_lines: usize,
    expanded_folds: &HashSet<String>,
) -> DiffResponse {
    build_text_diff_response_with_index(
        root_str,
        path,
        old_path,
        scope,
        before_text,
        after_text,
        None,
        omni_opt,
        engine,
        file_sha256,
        file_sig,
        file_reviewed,
        reviewed_chunks,
        context_lines,
        expanded_folds,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_text_diff_response_with_index(
    root_str: &str,
    path: &str,
    old_path: Option<&str>,
    scope: &str,
    before_text: &str,
    after_text: &str,
    index_snap: Option<IndexSnapshot<'_>>,
    omni_opt: Option<OmniDiffJson>,
    engine: String,
    file_sha256: Option<String>,
    file_sig: String,
    file_reviewed: bool,
    reviewed_chunks: &HashSet<String>,
    context_lines: usize,
    expanded_folds: &HashSet<String>,
) -> DiffResponse {
    let (before_lines, _) = split_lines_preserve(before_text);
    let (after_lines, _) = split_lines_preserve(after_text);

    enum AllStageState<'b> {
        AllStaged,
        AllUnstaged,
        Partial {
            idx_lines: Vec<&'b str>,
            ops: Vec<DiffOp>,
        },
    }
    let all_stage_state = if scope == "all" {
        match index_snap {
            None => AllStageState::AllUnstaged,
            Some(snap) if !snap.wt_exists => {
                if snap.text.is_none() {
                    AllStageState::AllStaged
                } else {
                    AllStageState::AllUnstaged
                }
            }
            Some(IndexSnapshot { text: None, .. }) => AllStageState::AllUnstaged,
            Some(IndexSnapshot {
                text: Some(idx_text),
                ..
            }) => {
                if idx_text == after_text {
                    AllStageState::AllStaged
                } else if idx_text == before_text {
                    AllStageState::AllUnstaged
                } else {
                    let (idx_lines, _) = split_lines_preserve(idx_text);
                    let hi = TextDiff::configure()
                        .algorithm(Algorithm::Patience)
                        .diff_slices(&before_lines, &idx_lines);
                    AllStageState::Partial {
                        ops: hi.ops().to_vec(),
                        idx_lines,
                    }
                }
            }
        }
    } else {
        AllStageState::AllUnstaged
    };

    let mut counts = ChangeCounts::default();
    let (language, encoding, summary, large_residual, mut b_ann, mut a_ann) =
        if let Some(omni) = omni_opt {
            let lang = omni
                .after
                .language
                .clone()
                .or_else(|| omni.before.language.clone());
            let enc = omni
                .after
                .encoding
                .clone()
                .or_else(|| omni.before.encoding.clone());
            let b = build_row_annotations(&before_lines, &omni.before.hunks, &mut counts, true);
            let a = build_row_annotations(&after_lines, &omni.after.hunks, &mut counts, false);
            (lang, enc, omni.summary, omni.large_residual, b, a)
        } else {
            (
                None,
                None,
                None,
                false,
                vec![RowAnnotations::default(); before_lines.len()],
                vec![RowAnnotations::default(); after_lines.len()],
            )
        };

    let text_diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_slices(&before_lines, &after_lines);
    let ops = text_diff.ops();

    let fallback_mode = engine == "fallback";
    if fallback_mode {
        for op in ops {
            match *op {
                DiffOp::Delete {
                    old_index,
                    old_len,
                    ..
                } => {
                    counts.deletions += old_len;
                    for r in old_index..(old_index + old_len) {
                        b_ann[r].has_delete = true;
                        b_ann[r].spans.push((0, before_lines[r].trim_end().len(), "d"));
                    }
                }
                DiffOp::Insert {
                    new_index,
                    new_len,
                    ..
                } => {
                    counts.insertions += new_len;
                    for r in new_index..(new_index + new_len) {
                        a_ann[r].has_insert = true;
                        a_ann[r].spans.push((0, after_lines[r].trim_end().len(), "i"));
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    counts.updates += old_len.max(new_len);
                    for r in old_index..(old_index + old_len) {
                        b_ann[r].has_delete = true;
                        b_ann[r].spans.push((0, before_lines[r].trim_end().len(), "d"));
                    }
                    for r in new_index..(new_index + new_len) {
                        a_ann[r].has_insert = true;
                        a_ann[r].spans.push((0, after_lines[r].trim_end().len(), "i"));
                    }
                }
                DiffOp::Equal { .. } => {}
            }
        }
    }

    #[derive(Debug, Clone)]
    struct Block {
        equal: bool,
        b_start: usize,
        b_end: usize,
        a_start: usize,
        a_end: usize,
    }

    let mut blocks: Vec<Block> = Vec::new();
    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                let mut i = 0;
                while i < len {
                    let marked =
                        b_ann[old_index + i].any_change() || a_ann[new_index + i].any_change();
                    let start = i;
                    i += 1;
                    while i < len
                        && (b_ann[old_index + i].any_change() || a_ann[new_index + i].any_change())
                            == marked
                    {
                        i += 1;
                    }
                    blocks.push(Block {
                        equal: !marked,
                        b_start: old_index + start,
                        b_end: old_index + i,
                        a_start: new_index + start,
                        a_end: new_index + i,
                    });
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => blocks.push(Block {
                equal: false,
                b_start: old_index,
                b_end: old_index + old_len,
                a_start: new_index,
                a_end: new_index,
            }),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => blocks.push(Block {
                equal: false,
                b_start: old_index,
                b_end: old_index,
                a_start: new_index,
                a_end: new_index + new_len,
            }),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => blocks.push(Block {
                equal: false,
                b_start: old_index,
                b_end: old_index + old_len,
                a_start: new_index,
                a_end: new_index + new_len,
            }),
        }
    }

    let mut merged: Vec<Block> = Vec::with_capacity(blocks.len());
    for blk in blocks {
        if let Some(last) = merged.last_mut()
            && last.equal == blk.equal
        {
            last.b_end = blk.b_end;
            last.a_end = blk.a_end;
            continue;
        }
        merged.push(blk);
    }

    let mut chunks: Vec<ChunkInfo> = Vec::new();
    let mut block_chunk_idx: Vec<Option<usize>> = vec![None; merged.len()];
    for (bi, blk) in merged.iter().enumerate() {
        if blk.equal {
            continue;
        }
        let idx = chunks.len();
        block_chunk_idx[bi] = Some(idx);

        let mut add = 0usize;
        let mut del = 0usize;
        let mut upd = 0usize;
        let mut mov = 0usize;
        let mut ref_text: Option<String> = None;
        let mut move_to: Option<(usize, usize)> = None;
        let mut move_from: Option<(usize, usize)> = None;

        for ann in &b_ann[blk.b_start..blk.b_end] {
            if ann.has_delete {
                del += 1;
            }
            if ann.has_update {
                upd += 1;
            }
            if ann.has_move {
                mov += 1;
            }
            if ref_text.is_none()
                && let Some(rl) = ann.ref_line
                && rl < before_lines.len()
                && rl < blk.b_start
            {
                ref_text = Some(before_lines[rl].trim().to_string());
            }
            if let (Some(lo), Some(hi)) = (ann.move_dest_min, ann.move_dest_max) {
                move_to = Some(match move_to {
                    None => (lo, hi),
                    Some((mlo, mhi)) => (mlo.min(lo), mhi.max(hi)),
                });
            }
        }
        for ann in &a_ann[blk.a_start..blk.a_end] {
            if ann.has_insert {
                add += 1;
            }
            if ann.has_update && blk.b_start == blk.b_end {
                upd += 1;
            }
            if ann.has_move {
                mov += 1;
            }
            if let Some(rl) = ann.ref_line
                && rl < after_lines.len()
                && rl < blk.a_start
            {
                ref_text = Some(after_lines[rl].trim().to_string());
            }
            if let (Some(lo), Some(hi)) = (ann.move_dest_min, ann.move_dest_max) {
                move_from = Some(match move_from {
                    None => (lo, hi),
                    Some((mlo, mhi)) => (mlo.min(lo), mhi.max(hi)),
                });
            }
        }

        let move_info = if let Some((lo, hi)) = move_to {
            Some(MoveInfo {
                direction: "to".to_string(),
                start_line: lo + 1,
                end_line: hi + 1,
                target_chunk: None,
            })
        } else {
            move_from.map(|(lo, hi)| MoveInfo {
                direction: "from".to_string(),
                start_line: lo + 1,
                end_line: hi + 1,
                target_chunk: None,
            })
        };

        let mut sig_input =
            format!("{path}:{}:{}\n", blk.b_end - blk.b_start, blk.a_end - blk.a_start);
        for l in &before_lines[blk.b_start..blk.b_end] {
            sig_input.push('-');
            sig_input.push_str(l);
            sig_input.push('\n');
        }
        for l in &after_lines[blk.a_start..blk.a_end] {
            sig_input.push('+');
            sig_input.push_str(l);
            sig_input.push('\n');
        }
        let signature = short_sha256(sig_input.as_bytes());
        let review_key = make_chunk_review_key(root_str, path, &signature);
        let reviewed = file_reviewed || reviewed_chunks.contains(&review_key);
        let patchable = before_lines[blk.b_start..blk.b_end] != after_lines[blk.a_start..blk.a_end];
        let staged = if scope == "staged" {
            true
        } else if scope == "all" && patchable {
            match &all_stage_state {
                AllStageState::AllStaged => true,
                AllStageState::AllUnstaged => false,
                AllStageState::Partial { idx_lines, ops } => {
                    map_hunk_to_index(ops, blk.b_start, blk.b_end).is_some_and(|(is, ie)| {
                        idx_lines[is..ie] == after_lines[blk.a_start..blk.a_end]
                    })
                }
            }
        } else {
            false
        };

        chunks.push(ChunkInfo {
            index: idx,
            b_start: blk.b_start,
            b_end: blk.b_end,
            a_start: blk.a_start,
            a_end: blk.a_end,
            patchable,
            staged,
            breadcrumb: ref_text.map(|s| {
                if s.chars().count() > 72 {
                    format!("{}…", s.chars().take(72).collect::<String>())
                } else {
                    s
                }
            }),
            move_info,
            signature,
            reviewed,
            add,
            del,
            upd,
            mov,
            unified_row: 0,
            split_row: 0,
        });
    }

    let chunk_bounds: Vec<(usize, usize, usize, usize)> = chunks
        .iter()
        .map(|c| (c.b_start, c.b_end, c.a_start, c.a_end))
        .collect();
    for c in &mut chunks {
        let Some(mi) = &mut c.move_info else { continue };
        let target_row0 = mi.start_line.saturating_sub(1);
        mi.target_chunk = if mi.direction == "to" {
            chunk_bounds
                .iter()
                .position(|&(_, _, a0, a1)| target_row0 >= a0 && target_row0 < a1.max(a0 + 1))
        } else {
            chunk_bounds
                .iter()
                .position(|&(b0, b1, _, _)| target_row0 >= b0 && target_row0 < b1.max(b0 + 1))
        };
    }

    let mut unified_rows: Vec<UnifiedRow> = Vec::new();
    let mut split_rows: Vec<SplitRow> = Vec::new();
    let mut max_unified_chars = 48usize;
    let mut max_split_chars = 36usize;
    let mut truncated = false;
    let num_blocks = merged.len();

    for (bi, blk) in merged.iter().enumerate() {
        if unified_rows.len() >= MAX_RENDER_ROWS || split_rows.len() >= MAX_RENDER_ROWS {
            truncated = true;
            break;
        }
        if blk.equal {
            let len = blk.b_end - blk.b_start;
            if len == 0 {
                continue;
            }
            let has_prev_change = bi > 0;
            let has_next_change = bi + 1 < num_blocks;
            if !has_prev_change && !has_next_change {
                let fold_id = format!("f:{}:{}", blk.b_start, blk.a_start);
                if !expanded_folds.contains(&fold_id) {
                    let msg = format!("… {len} unchanged line{} …", if len == 1 { "" } else { "s" });
                    unified_rows.push(UnifiedRow {
                        key: format!("elide:{fold_id}"),
                        kind: "elide".to_string(),
                        chunk_idx: None,
                        fold_id: Some(fold_id.clone()),
                        count: Some(len),
                        text: Some(msg.clone()),
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        old_no: None,
                        new_no: None,
                        marker: None,
                        row_bg: None,
                        segs: Vec::new(),
                    });
                    split_rows.push(SplitRow {
                        key: format!("elide:{fold_id}"),
                        kind: "elide".to_string(),
                        chunk_idx: None,
                        fold_id: Some(fold_id),
                        count: Some(len),
                        text: Some(msg),
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        left: None,
                        right: None,
                    });
                    continue;
                }
            }

            let lead = if has_prev_change { context_lines } else { 0 };
            let trail = if has_next_change { context_lines } else { 0 };
            let fold_id = format!(
                "f:{}:{}",
                blk.b_start + lead.min(len),
                blk.a_start + lead.min(len)
            );
            let should_fold = len > lead + trail + 1 && !expanded_folds.contains(&fold_id);

            let push_ctx = |offset: usize,
                            u_rows: &mut Vec<UnifiedRow>,
                            s_rows: &mut Vec<SplitRow>,
                            max_u: &mut usize,
                            max_s: &mut usize| {
                let br = blk.b_start + offset;
                let ar = blk.a_start + offset;
                let (segs, dlen) = slice_line_segments(after_lines[ar], &[]);
                *max_u = (*max_u).max(dlen);
                *max_s = (*max_s).max(dlen);
                u_rows.push(UnifiedRow {
                    key: format!("ctx:{br}:{ar}"),
                    kind: "line".to_string(),
                    chunk_idx: None,
                    fold_id: None,
                    count: None,
                    text: None,
                    breadcrumb: None,
                    move_hint: None,
                    target_chunk: None,
                    staged: None,
                    reviewed: None,
                    old_no: Some(br + 1),
                    new_no: Some(ar + 1),
                    marker: Some(" ".to_string()),
                    row_bg: None,
                    segs: segs.clone(),
                });
                s_rows.push(SplitRow {
                    key: format!("ctx:{br}:{ar}"),
                    kind: "pair".to_string(),
                    chunk_idx: None,
                    fold_id: None,
                    count: None,
                    text: None,
                    breadcrumb: None,
                    move_hint: None,
                    target_chunk: None,
                    staged: None,
                    reviewed: None,
                    left: Some(SplitCell {
                        line_no: br + 1,
                        marker: " ".to_string(),
                        row_bg: None,
                        segs: segs.clone(),
                    }),
                    right: Some(SplitCell {
                        line_no: ar + 1,
                        marker: " ".to_string(),
                        row_bg: None,
                        segs,
                    }),
                });
            };

            if should_fold {
                for off in 0..lead {
                    push_ctx(
                        off,
                        &mut unified_rows,
                        &mut split_rows,
                        &mut max_unified_chars,
                        &mut max_split_chars,
                    );
                }
                if has_prev_change {
                    unified_rows.push(UnifiedRow {
                        key: format!("spacer:pre:{fold_id}"),
                        kind: "spacer".to_string(),
                        chunk_idx: None,
                        fold_id: None,
                        count: None,
                        text: None,
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        old_no: None,
                        new_no: None,
                        marker: None,
                        row_bg: None,
                        segs: Vec::new(),
                    });
                    split_rows.push(SplitRow {
                        key: format!("spacer:pre:{fold_id}"),
                        kind: "spacer".to_string(),
                        chunk_idx: None,
                        fold_id: None,
                        count: None,
                        text: None,
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        left: None,
                        right: None,
                    });
                }
                let hidden = len - lead - trail;
                let msg = format!(
                    "… {hidden} unchanged line{} (click to expand) …",
                    if hidden == 1 { "" } else { "s" }
                );
                unified_rows.push(UnifiedRow {
                    key: format!("elide:{fold_id}"),
                    kind: "elide".to_string(),
                    chunk_idx: None,
                    fold_id: Some(fold_id.clone()),
                    count: Some(hidden),
                    text: Some(msg.clone()),
                    breadcrumb: None,
                    move_hint: None,
                    target_chunk: None,
                    staged: None,
                    reviewed: None,
                    old_no: None,
                    new_no: None,
                    marker: None,
                    row_bg: None,
                    segs: Vec::new(),
                });
                split_rows.push(SplitRow {
                    key: format!("elide:{fold_id}"),
                    kind: "elide".to_string(),
                    chunk_idx: None,
                    fold_id: Some(fold_id.clone()),
                    count: Some(hidden),
                    text: Some(msg),
                    breadcrumb: None,
                    move_hint: None,
                    target_chunk: None,
                    staged: None,
                    reviewed: None,
                    left: None,
                    right: None,
                });
                if has_next_change {
                    unified_rows.push(UnifiedRow {
                        key: format!("spacer:post:{fold_id}"),
                        kind: "spacer".to_string(),
                        chunk_idx: None,
                        fold_id: None,
                        count: None,
                        text: None,
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        old_no: None,
                        new_no: None,
                        marker: None,
                        row_bg: None,
                        segs: Vec::new(),
                    });
                    split_rows.push(SplitRow {
                        key: format!("spacer:post:{fold_id}"),
                        kind: "spacer".to_string(),
                        chunk_idx: None,
                        fold_id: None,
                        count: None,
                        text: None,
                        breadcrumb: None,
                        move_hint: None,
                        target_chunk: None,
                        staged: None,
                        reviewed: None,
                        left: None,
                        right: None,
                    });
                }
                for off in (len - trail)..len {
                    push_ctx(
                        off,
                        &mut unified_rows,
                        &mut split_rows,
                        &mut max_unified_chars,
                        &mut max_split_chars,
                    );
                }
            } else {
                for off in 0..len {
                    push_ctx(
                        off,
                        &mut unified_rows,
                        &mut split_rows,
                        &mut max_unified_chars,
                        &mut max_split_chars,
                    );
                }
            }
            continue;
        }

        let ci = block_chunk_idx[bi].unwrap();
        let u_idx = unified_rows.len();
        let s_idx = split_rows.len();
        chunks[ci].unified_row = u_idx;
        chunks[ci].split_row = s_idx;

        let mut left_cells: Vec<SplitCell> = Vec::with_capacity(blk.b_end - blk.b_start);
        let mut right_cells: Vec<SplitCell> = Vec::with_capacity(blk.a_end - blk.a_start);

        for br in blk.b_start..blk.b_end {
            let ann = &b_ann[br];
            let side_spans: Vec<(usize, usize, &'static str)> = ann
                .spans
                .iter()
                .filter(|&&(_, _, op)| op != "m")
                .map(|&(s, e, op)| (s, e, if op == "u" { "d" } else { op }))
                .collect();
            let (segs, dlen) = slice_line_segments(before_lines[br], &side_spans);
            max_unified_chars = max_unified_chars.max(dlen);
            max_split_chars = max_split_chars.max(dlen);
            let marker = ann.marker(true);
            let row_bg = ann.row_bg(true);
            unified_rows.push(UnifiedRow {
                key: format!("u:{ci}:b:{br}"),
                kind: "line".to_string(),
                chunk_idx: Some(ci),
                fold_id: None,
                count: None,
                text: None,
                breadcrumb: None,
                move_hint: None,
                target_chunk: None,
                staged: None,
                reviewed: None,
                old_no: Some(br + 1),
                new_no: None,
                marker: Some(marker.clone()),
                row_bg: row_bg.clone(),
                segs: segs.clone(),
            });
            left_cells.push(SplitCell {
                line_no: br + 1,
                marker,
                row_bg,
                segs,
            });
        }

        for ar in blk.a_start..blk.a_end {
            let ann = &a_ann[ar];
            let side_spans: Vec<(usize, usize, &'static str)> = ann
                .spans
                .iter()
                .filter(|&&(_, _, op)| op != "m")
                .map(|&(s, e, op)| (s, e, if op == "u" { "i" } else { op }))
                .collect();
            let (segs, dlen) = slice_line_segments(after_lines[ar], &side_spans);
            max_unified_chars = max_unified_chars.max(dlen);
            max_split_chars = max_split_chars.max(dlen);
            let marker = ann.marker(false);
            let row_bg = ann.row_bg(false);
            unified_rows.push(UnifiedRow {
                key: format!("u:{ci}:a:{ar}"),
                kind: "line".to_string(),
                chunk_idx: Some(ci),
                fold_id: None,
                count: None,
                text: None,
                breadcrumb: None,
                move_hint: None,
                target_chunk: None,
                staged: None,
                reviewed: None,
                old_no: None,
                new_no: Some(ar + 1),
                marker: Some(marker.clone()),
                row_bg: row_bg.clone(),
                segs: segs.clone(),
            });
            right_cells.push(SplitCell {
                line_no: ar + 1,
                marker,
                row_bg,
                segs,
            });
        }

        let pair_len = left_cells.len().max(right_cells.len());
        let mut l_iter = left_cells.into_iter();
        let mut r_iter = right_cells.into_iter();
        for pi in 0..pair_len {
            split_rows.push(SplitRow {
                key: format!("s:{ci}:{pi}"),
                kind: "pair".to_string(),
                chunk_idx: Some(ci),
                fold_id: None,
                count: None,
                text: None,
                breadcrumb: None,
                move_hint: None,
                target_chunk: None,
                staged: None,
                reviewed: None,
                left: l_iter.next(),
                right: r_iter.next(),
            });
        }
    }

    DiffResponse {
        path: path.to_string(),
        old_path: old_path.map(str::to_string),
        scope: scope.to_string(),
        language,
        encoding,
        summary,
        large_residual,
        engine,
        binary: false,
        content: None,
        picture_media: None,
        file_sha256,
        file_sig,
        file_reviewed,
        counts,
        chunks,
        unified_rows,
        split_rows,
        max_unified_chars,
        max_split_chars,
        truncated,
    }
}

// ---- Selection, Line Wrapping, and AI Prompt Formatting ---------------------

pub fn segs_to_plain_text(segs: &[TextSegment]) -> String {
    let mut s = String::new();
    for seg in segs {
        s.push_str(&seg.t);
    }
    s
}

pub fn diff_row_prefix(marker: Option<&str>, row_bg: Option<&str>) -> &'static str {
    let m = marker.unwrap_or(" ");
    let bg = row_bg.unwrap_or("");
    if m == "+" || bg == "i" {
        "+"
    } else if m == "-" || bg == "d" {
        "-"
    } else if m == "~" || m == "⇄" {
        if bg == "d" {
            "-"
        } else if bg == "i" {
            "+"
        } else {
            "~"
        }
    } else {
        " "
    }
}

fn cap_snippet_lines(lines: &[String]) -> String {
    if lines.len() <= MAX_SNIPPET_LINES {
        lines.join("\n")
    } else {
        let rest = lines.len() - MAX_SNIPPET_LINES;
        let mut out: Vec<String> = lines[..MAX_SNIPPET_LINES].to_vec();
        out.push(format!("  ... ({rest} more lines)"));
        out.join("\n")
    }
}

#[allow(dead_code)]
pub fn wrap_segments(segs: &[TextSegment], max_cols: usize) -> Vec<Vec<TextSegment>> {
    let limit = max_cols.max(16);
    if segs.is_empty() {
        return vec![vec![TextSegment {
            t: " ".to_string(),
            op: None,
        }]];
    }

    let mut chars: Vec<char> = Vec::new();
    let mut ops: Vec<Option<String>> = Vec::new();
    for seg in segs {
        for ch in seg.t.chars() {
            chars.push(ch);
            ops.push(seg.op.clone());
        }
    }
    if chars.len() <= limit {
        return vec![segs.to_vec()];
    }

    let min_break_offset = (limit * 58 / 100).max(10);
    let mut lines: Vec<Vec<TextSegment>> = Vec::new();
    let mut pos = 0usize;
    while pos < chars.len() {
        let mut end = (pos + limit).min(chars.len());
        if end < chars.len() {
            for brk in ((pos + min_break_offset)..=end).rev() {
                if matches!(
                    chars[brk - 1],
                    ' ' | '\t' | ',' | ';' | '/' | '(' | '[' | '{' | '|' | '&'
                ) {
                    end = brk;
                    break;
                }
            }
        }

        let mut line_segs: Vec<TextSegment> = Vec::new();
        let mut cur_op = ops[pos].clone();
        let mut cur_text = String::from(chars[pos]);
        for k in (pos + 1)..end {
            if ops[k] == cur_op {
                cur_text.push(chars[k]);
            } else {
                line_segs.push(TextSegment {
                    t: cur_text,
                    op: cur_op,
                });
                cur_op = ops[k].clone();
                cur_text = String::from(chars[k]);
            }
        }
        line_segs.push(TextSegment {
            t: cur_text,
            op: cur_op,
        });
        lines.push(line_segs);
        pos = end;
    }

    lines
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SelectionTarget {
    pub kind: String,
    pub path: String,
    pub side: String, // "both" | "before" | "after"
    pub start_idx: usize,
    pub end_idx: usize,
    pub anchor_idx: usize,
    pub chunk_idx: Option<usize>,
    pub breadcrumb: Option<String>,
    pub short_label: String,
    pub short_ref: String,
    pub line_spec: String,
    pub snippet: String,
    pub unified_keys: HashSet<String>,
    pub split_keys: HashSet<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AiQueueItem {
    pub id: usize,
    pub path: String,
    pub scope: String,
    pub commit_hash: Option<String>,
    pub side: String,
    pub breadcrumb: Option<String>,
    pub short_label: String,
    pub short_ref: String,
    pub line_spec: String,
    pub snippet: String,
    pub note: String,
    pub unified_keys: HashSet<String>,
    pub split_keys: HashSet<String>,
}

pub fn resolve_selection_details(
    diff: &DiffResponse,
    split_mode: bool,
    start_idx: usize,
    end_idx: usize,
    side: &str,
    anchor_idx: Option<usize>,
) -> Option<SelectionTarget> {
    if diff.binary {
        return None;
    }
    let len = if split_mode {
        diff.split_rows.len()
    } else {
        diff.unified_rows.len()
    };
    if len == 0 {
        return None;
    }

    let lo = start_idx.min(end_idx).min(len - 1);
    let hi = start_idx.max(end_idx).min(len - 1);

    let mut effective_side = if split_mode { side } else { "both" };
    if split_mode && effective_side == "before" {
        let any_left = (lo..=hi).any(|i| {
            diff.split_rows[i].kind == "pair" && diff.split_rows[i].left.is_some()
        });
        if !any_left {
            effective_side = "both";
        }
    } else if split_mode && effective_side == "after" {
        let any_right = (lo..=hi).any(|i| {
            diff.split_rows[i].kind == "pair" && diff.split_rows[i].right.is_some()
        });
        if !any_right {
            effective_side = "both";
        }
    }

    let mut old_nos: Vec<usize> = Vec::new();
    let mut new_nos: Vec<usize> = Vec::new();
    let mut snippet_lines: Vec<String> = Vec::new();
    let mut chunk_idx: Option<usize> = None;
    let mut unified_keys: HashSet<String> = HashSet::new();
    let mut split_keys: HashSet<String> = HashSet::new();

    if split_mode {
        let code_rows: Vec<&SplitRow> = (lo..=hi)
            .map(|i| &diff.split_rows[i])
            .filter(|r| r.kind == "pair")
            .collect();
        if code_rows.is_empty() {
            return None;
        }
        for r in &code_rows {
            split_keys.insert(r.key.clone());
            if chunk_idx.is_none() {
                chunk_idx = r.chunk_idx;
            }
        }
        if effective_side == "before" {
            for r in &code_rows {
                if let Some(l) = &r.left {
                    old_nos.push(l.line_no);
                    let pfx = diff_row_prefix(Some(&l.marker), l.row_bg.as_deref());
                    snippet_lines.push(format!(
                        "{pfx} {:>4} | {}",
                        l.line_no,
                        segs_to_plain_text(&l.segs)
                    ));
                }
            }
        } else if effective_side == "after" {
            for r in &code_rows {
                if let Some(ri) = &r.right {
                    new_nos.push(ri.line_no);
                    let pfx = diff_row_prefix(Some(&ri.marker), ri.row_bg.as_deref());
                    snippet_lines.push(format!(
                        "{pfx} {:>4} | {}",
                        ri.line_no,
                        segs_to_plain_text(&ri.segs)
                    ));
                }
            }
        } else {
            let mut pending_before: Vec<String> = Vec::new();
            let mut pending_after: Vec<String> = Vec::new();
            let flush = |out: &mut Vec<String>,
                         pb: &mut Vec<String>,
                         pa: &mut Vec<String>| {
                out.append(pb);
                out.append(pa);
            };
            for r in &code_rows {
                if let Some(l) = &r.left {
                    old_nos.push(l.line_no);
                }
                if let Some(ri) = &r.right {
                    new_nos.push(ri.line_no);
                }
                if let (Some(l), Some(ri)) = (&r.left, &r.right)
                    && l.marker == " "
                    && ri.marker == " "
                    && l.row_bg.is_none()
                    && ri.row_bg.is_none()
                {
                    flush(&mut snippet_lines, &mut pending_before, &mut pending_after);
                    snippet_lines.push(format!(
                        "  {:>4} | {}",
                        ri.line_no,
                        segs_to_plain_text(&ri.segs)
                    ));
                } else {
                    if let Some(l) = &r.left {
                        let p = diff_row_prefix(Some(&l.marker), l.row_bg.as_deref());
                        let p = if p == " " { "-" } else { p };
                        pending_before.push(format!(
                            "{p} {:>4} | {}",
                            l.line_no,
                            segs_to_plain_text(&l.segs)
                        ));
                    }
                    if let Some(ri) = &r.right {
                        let p = diff_row_prefix(Some(&ri.marker), ri.row_bg.as_deref());
                        let p = if p == " " { "+" } else { p };
                        pending_after.push(format!(
                            "{p} {:>4} | {}",
                            ri.line_no,
                            segs_to_plain_text(&ri.segs)
                        ));
                    }
                }
            }
            flush(&mut snippet_lines, &mut pending_before, &mut pending_after);
        }
    } else {
        let code_rows: Vec<&UnifiedRow> = (lo..=hi)
            .map(|i| &diff.unified_rows[i])
            .filter(|r| r.kind == "line")
            .collect();
        if code_rows.is_empty() {
            return None;
        }
        for r in &code_rows {
            unified_keys.insert(r.key.clone());
            if chunk_idx.is_none() {
                chunk_idx = r.chunk_idx;
            }
            if let Some(o) = r.old_no {
                old_nos.push(o);
            }
            if let Some(n) = r.new_no {
                new_nos.push(n);
            }
            let pfx = diff_row_prefix(r.marker.as_deref(), r.row_bg.as_deref());
            let ln = r.new_no.or(r.old_no).unwrap_or(0);
            snippet_lines.push(format!("{pfx} {:>4} | {}", ln, segs_to_plain_text(&r.segs)));
        }
    }

    let min_new = new_nos.iter().copied().min();
    let max_new = new_nos.iter().copied().max();
    let min_old = old_nos.iter().copied().min();
    let max_old = old_nos.iter().copied().max();

    if split_mode {
        for u in &diff.unified_rows {
            if u.kind != "line" {
                continue;
            }
            let match_new = effective_side != "before"
                && min_new.is_some_and(|mn| {
                    u.new_no
                        .is_some_and(|n| n >= mn && n <= max_new.unwrap_or(mn))
                });
            let match_old = effective_side != "after"
                && min_old.is_some_and(|mo| {
                    u.old_no
                        .is_some_and(|o| o >= mo && o <= max_old.unwrap_or(mo))
                })
                && (effective_side == "before" || u.new_no.is_none());
            if match_new || match_old {
                unified_keys.insert(u.key.clone());
            }
        }
    } else {
        for s in &diff.split_rows {
            if s.kind != "pair" {
                continue;
            }
            let r_no = s.right.as_ref().map(|c| c.line_no);
            let l_no = s.left.as_ref().map(|c| c.line_no);
            let match_new = min_new.is_some_and(|mn| {
                r_no.is_some_and(|n| n >= mn && n <= max_new.unwrap_or(mn))
            });
            let match_old = min_old.is_some_and(|mo| {
                l_no.is_some_and(|o| o >= mo && o <= max_old.unwrap_or(mo)) && r_no.is_none()
            });
            if match_new || match_old {
                split_keys.insert(s.key.clone());
            }
        }
    }

    let chunk = chunk_idx.and_then(|ci| diff.chunks.get(ci));
    let breadcrumb = chunk.and_then(|c| c.breadcrumb.clone());
    let file_name = diff.path.rsplit('/').next().unwrap_or(&diff.path);
    let mut short_label = "lines".to_string();
    let mut short_ref = file_name.to_string();
    let mut line_spec = diff.path.clone();

    if let (Some(mn), Some(mx)) = (min_new, max_new)
        && effective_side != "before"
    {
        let range_str = if mn == mx {
            format!("{mn}")
        } else {
            format!("{mn}-{mx}")
        };
        let ui_range = if mn == mx {
            format!("L{mn}")
        } else {
            format!("L{mn}–{mx}")
        };
        short_label = ui_range.clone();
        short_ref = format!("{file_name}:{ui_range}");
        line_spec = format!("{}:{range_str}", diff.path);
    } else if let (Some(mo), Some(mxo)) = (min_old, max_old) {
        let range_str = if mo == mxo {
            format!("{mo}")
        } else {
            format!("{mo}-{mxo}")
        };
        let ui_range = if mo == mxo {
            format!("L{mo}")
        } else {
            format!("L{mo}–{mxo}")
        };
        let near_cur = chunk.map(|c| (c.a_start + 1).max(1));
        short_label = format!("{ui_range} (before)");
        short_ref = format!("{file_name}:{ui_range} (before)");
        line_spec = if let Some(nc) = near_cur {
            format!("{} (before L{range_str}, near current L{nc})", diff.path)
        } else {
            format!("{} (before L{range_str})", diff.path)
        };
    }

    Some(SelectionTarget {
        kind: "lines".to_string(),
        path: diff.path.clone(),
        side: effective_side.to_string(),
        start_idx: lo,
        end_idx: hi,
        anchor_idx: anchor_idx.unwrap_or(lo),
        chunk_idx,
        breadcrumb,
        short_label,
        short_ref,
        line_spec,
        snippet: cap_snippet_lines(&snippet_lines),
        unified_keys,
        split_keys,
    })
}

pub fn resolve_active_chunk_target(diff: &DiffResponse, active_chunk: usize) -> SelectionTarget {
    let file_name = diff.path.rsplit('/').next().unwrap_or(&diff.path);
    if !diff.binary && !diff.chunks.is_empty() {
        let idx = active_chunk.min(diff.chunks.len() - 1);
        let c = &diff.chunks[idx];
        let mut snippet_lines: Vec<String> = Vec::new();
        let mut unified_keys: HashSet<String> = HashSet::new();
        let mut split_keys: HashSet<String> = HashSet::new();

        for r in &diff.unified_rows {
            if r.kind == "line" && r.chunk_idx == Some(idx) {
                unified_keys.insert(r.key.clone());
                let pfx = diff_row_prefix(r.marker.as_deref(), r.row_bg.as_deref());
                let ln = r.new_no.or(r.old_no).unwrap_or(0);
                snippet_lines.push(format!("{pfx} {:>4} | {}", ln, segs_to_plain_text(&r.segs)));
            }
        }
        for s in &diff.split_rows {
            if s.kind == "pair" && s.chunk_idx == Some(idx) {
                split_keys.insert(s.key.clone());
            }
        }

        let mut line_spec = diff.path.clone();
        let mut short_ref = format!("{file_name} · Hunk {}/{}", idx + 1, diff.chunks.len());
        if c.a_end > c.a_start {
            let s = c.a_start + 1;
            let e = c.a_end;
            line_spec = if s == e {
                format!("{}:{s}", diff.path)
            } else {
                format!("{}:{s}-{e}", diff.path)
            };
            short_ref = format!(
                "{file_name}:{} (Hunk {}/{})",
                if s == e {
                    format!("L{s}")
                } else {
                    format!("L{s}–{e}")
                },
                idx + 1,
                diff.chunks.len()
            );
        } else if c.b_end > c.b_start {
            let s = c.b_start + 1;
            let e = c.b_end;
            let near_cur = (c.a_start + 1).max(1);
            line_spec = format!(
                "{} (deleted before L{}, near current L{near_cur})",
                diff.path,
                if s == e {
                    format!("{s}")
                } else {
                    format!("{s}-{e}")
                }
            );
        }

        return SelectionTarget {
            kind: "chunk".to_string(),
            path: diff.path.clone(),
            side: "both".to_string(),
            start_idx: c.unified_row,
            end_idx: c.unified_row,
            anchor_idx: c.unified_row,
            chunk_idx: Some(idx),
            breadcrumb: c.breadcrumb.clone(),
            short_label: format!("Hunk {}/{}", idx + 1, diff.chunks.len()),
            short_ref,
            line_spec,
            snippet: cap_snippet_lines(&snippet_lines),
            unified_keys,
            split_keys,
        };
    }

    SelectionTarget {
        kind: "file".to_string(),
        path: diff.path.clone(),
        side: "both".to_string(),
        start_idx: 0,
        end_idx: 0,
        anchor_idx: 0,
        chunk_idx: None,
        breadcrumb: None,
        short_label: "File".to_string(),
        short_ref: file_name.to_string(),
        line_spec: diff.path.clone(),
        snippet: String::new(),
        unified_keys: HashSet::new(),
        split_keys: HashSet::new(),
    }
}

pub fn format_ai_review_prompt(
    items: &[AiQueueItem],
    default_scope: &str,
    default_commit: Option<&str>,
) -> String {
    if items.is_empty() {
        return String::new();
    }
    let scope_desc = |it: &AiQueueItem| -> String {
        let s = if it.scope.is_empty() {
            default_scope
        } else {
            it.scope.as_str()
        };
        let ch = it.commit_hash.as_deref().or(default_commit);
        if s == "commits" || s == "commit" {
            if let Some(h) = ch {
                return format!("commit {}", &h[..h.len().min(7)]);
            }
            return "commit diff".to_string();
        }
        if s == "staged" {
            return "staged diff".to_string();
        }
        if s == "unstaged" {
            return "unstaged diff".to_string();
        }
        "working-tree diff".to_string()
    };

    if items.len() == 1 {
        let it = &items[0];
        let mut ctx = vec![scope_desc(it)];
        if let Some(bc) = &it.breadcrumb {
            ctx.push(format!("@ {bc}"));
        }
        let mut out = vec![format!(
            "Review request on `{}` ({}):",
            it.line_spec,
            ctx.join(", ")
        )];
        if !it.snippet.is_empty() {
            out.push("```diff".to_string());
            out.push(it.snippet.clone());
            out.push("```".to_string());
        }
        out.push(it.note.clone());
        return out.join("\n");
    }

    let mut out = vec![format!(
        "Please address the following {} code review comments in this repository:",
        items.len()
    )];
    for (idx, it) in items.iter().enumerate() {
        let mut ctx = vec![scope_desc(it)];
        if let Some(bc) = &it.breadcrumb {
            ctx.push(format!("@ {bc}"));
        }
        out.push(String::new());
        out.push(format!(
            "### {}. `{}` ({})",
            idx + 1,
            it.line_spec,
            ctx.join(", ")
        ));
        if !it.snippet.is_empty() {
            out.push("```diff".to_string());
            out.push(it.snippet.clone());
            out.push("```".to_string());
        }
        out.push(format!("Instruction: {}", it.note));
    }
    out.join("\n")
}

pub fn pick_next_path_after_mutation(
    list: &[FileEntry],
    prev_list: &[FileEntry],
    prev_selected: Option<&str>,
    prev_idx: usize,
    advance_from_stage: bool,
) -> Option<String> {
    if list.is_empty() {
        return None;
    }
    let Some(prev_sel) = prev_selected else {
        return Some(
            list.iter()
                .find(|f| f.unstaged_part)
                .unwrap_or(&list[0])
                .path
                .clone(),
        );
    };

    let still_entry = list.iter().find(|f| f.path == prev_sel);
    if advance_from_stage {
        if still_entry.is_some() {
            if let Some(cur_idx) = list.iter().position(|f| f.path == prev_sel) {
                for off in 1..list.len() {
                    let cand = &list[(cur_idx + off) % list.len()];
                    if cand.unstaged_part {
                        return Some(cand.path.clone());
                    }
                }
                if cur_idx + 1 < list.len() {
                    return Some(list[cur_idx + 1].path.clone());
                }
            }
            return Some(prev_sel.to_string());
        }

        if !prev_list.is_empty() {
            for off in 1..prev_list.len() {
                let cand_path = &prev_list[(prev_idx + off) % prev_list.len()].path;
                if let Some(m) = list.iter().find(|f| &f.path == cand_path)
                    && m.unstaged_part
                {
                    return Some(m.path.clone());
                }
            }
            for off in 1..prev_list.len() {
                let cand_path = &prev_list[(prev_idx + off) % prev_list.len()].path;
                if list.iter().any(|f| &f.path == cand_path) {
                    return Some(cand_path.clone());
                }
            }
        }
        let clamped = prev_idx.min(list.len() - 1);
        return Some(list[clamped].path.clone());
    }

    if still_entry.is_none() {
        if !prev_list.is_empty() {
            for off in 1..prev_list.len() {
                let cand_path = &prev_list[(prev_idx + off) % prev_list.len()].path;
                if list.iter().any(|f| &f.path == cand_path) {
                    return Some(cand_path.clone());
                }
            }
        }
        let clamped = prev_idx.min(list.len() - 1);
        return Some(list[clamped].path.clone());
    }

    Some(prev_sel.to_string())
}
