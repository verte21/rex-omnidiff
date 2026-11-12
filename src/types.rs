use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- OmniDiff `--mode json` schema types ------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct OmniDiffJson {
    pub before: OmniSide,
    pub after: OmniSide,
    #[serde(default)]
    pub large_residual: bool,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub binary: Option<bool>,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub picture: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OmniSide {
    #[allow(dead_code)]
    pub path: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub hunks: Vec<OmniHunk>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OmniHunk {
    pub operation: String,
    pub range: OmniRange,
    #[serde(default)]
    pub move_target: Option<OmniRange>,
    #[serde(default)]
    pub reference_line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct OmniRange {
    pub start_row: usize,
    pub start_column: usize,
    pub end_row: usize,
    pub end_column: usize,
}

impl OmniRange {
    pub fn is_empty(&self) -> bool {
        self.start_row == self.end_row && self.start_column == self.end_column
    }

    pub fn columns_on_row(&self, row: usize, row_len: usize) -> Option<(usize, usize)> {
        if row < self.start_row || row > self.end_row {
            return None;
        }
        let start = if row == self.start_row {
            self.start_column.min(row_len)
        } else {
            0
        };
        let end = if row == self.end_row {
            self.end_column.min(row_len)
        } else {
            row_len
        };
        if start >= end {
            None
        } else {
            Some((start, end))
        }
    }

    pub fn last_touched_row(&self) -> usize {
        if self.end_column == 0 {
            self.end_row.saturating_sub(1)
        } else {
            self.end_row
        }
    }
}

// ---- RPC & UI State Responses -----------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub root: String,
    pub branch: Option<String>,
    pub has_head: bool,
    pub omnidiff_bin: Option<String>,
    pub all: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    pub staged: Vec<FileEntry>,
    pub commits: Vec<CommitEntry>,
    pub undo_top: Option<UndoSummary>,
    pub undo_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub untracked: bool,
    pub add: usize,
    pub del: usize,
    pub binary: bool,
    pub staged_part: bool,
    pub unstaged_part: bool,
    pub reviewed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEntry {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitFilesResponse {
    pub hash: String,
    pub base: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoSummary {
    pub id: String,
    pub label: String,
    pub path: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeCounts {
    pub insertions: usize,
    pub deletions: usize,
    pub updates: usize,
    pub moves: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PictureMedia {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_rel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_rel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_abs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_abs: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveInfo {
    pub direction: String, // "to" or "from"
    pub start_line: usize, // 1-indexed
    pub end_line: usize,   // 1-indexed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_chunk: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkInfo {
    pub index: usize,
    pub b_start: usize,
    pub b_end: usize,
    pub a_start: usize,
    pub a_end: usize,
    pub patchable: bool,
    pub staged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_info: Option<MoveInfo>,
    pub signature: String,
    pub reviewed: bool,
    pub add: usize,
    pub del: usize,
    pub upd: usize,
    pub mov: usize,
    pub unified_row: usize,
    pub split_row: usize,
}

/// Compact inline styled text run (`op`: None, Some("i"), Some("d"), Some("u"), Some("m")).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSegment {
    pub t: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedRow {
    pub key: String,
    pub kind: String, // "chunk" | "elide" | "line"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fold_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_chunk: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_no: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_no: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segs: Vec<TextSegment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitCell {
    pub line_no: usize,
    pub marker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segs: Vec<TextSegment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitRow {
    pub key: String,
    pub kind: String, // "chunk" | "elide" | "pair"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fold_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_chunk: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<SplitCell>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<SplitCell>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffResponse {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub large_residual: bool,
    pub engine: String,
    pub binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture_media: Option<PictureMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    pub file_sig: String,
    pub file_reviewed: bool,
    pub counts: ChangeCounts,
    pub chunks: Vec<ChunkInfo>,
    pub unified_rows: Vec<UnifiedRow>,
    pub split_rows: Vec<SplitRow>,
    pub max_unified_chars: usize,
    pub max_split_chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TuiLaunchSpec {
    pub argv: Vec<String>,
    pub cwd: String,
    pub title: String,
}
