use std::collections::HashSet;
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::actions::{
    cleanup_scratch, commit_changes, discard_file, pop_undo, prepare_tui_launch, revert_chunk,
    stage_chunk, stage_file, toggle_review, unstage_chunk, unstage_file,
};
use crate::diff_engine::{
    AiQueueItem, DiffParams, SelectionTarget, compute_diff, format_ai_review_prompt,
    pick_next_path_after_mutation, resolve_active_chunk_target, resolve_selection_details,
};
use crate::git::{git_write, load_commit_files, load_status, repo_root};
use crate::kitty::{KittyImageMode, KittyPreviewCache, clear_kitty_images};
use crate::rex_bridge::{
    RexAgentChoice, RexSessionContext, close_self_in_rex, detect_session_context,
    discover_rex_agents, resolve_live_followed_cwd, send_prompt_to_rex_agent,
};
use crate::theme::{
    LineSyntaxState, ThemePalette, detect_rex_active_theme_id, load_active_rex_palette,
    tokenize_syntax_segment,
};
use crate::types::{DiffResponse, FileEntry, SplitCell, TextSegment};

#[derive(Debug, Clone)]
pub struct UiOptions {
    pub cwd: PathBuf,
    pub rex_home: PathBuf,
    pub mode: String, // "float" | "dock"
    pub source_block: Option<String>,
    pub initial_scope: Option<String>,
    pub initial_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewModeOverride {
    Auto,
    Unified,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerMode {
    None,
    AskAi,
    Commit,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum HitAction {
    SelectScope(String),
    SelectCommit(String),
    SelectFile(usize),
    StageFile(usize),
    UnstageFile(usize),
    DiscardFile(usize),
    StageAll,
    UnstageAll,
    UndoTop,
    CyclePreset,
    ToggleWrap,
    ToggleViewMode,
    ToggleSidebar,
    LaunchTui,
    Refresh,
    CloseApp,
    PrevHunk,
    NextHunk,
    JumpMove(usize),
    StageActiveHunk,
    UnstageActiveHunk,
    RevertActiveHunk,
    StageHunkIdx(usize),
    UnstageHunkIdx(usize),
    RevertHunkIdx(usize),
    ToggleFileReview,
    ToggleChunkReview(usize),
    ExpandFold(String),
    SelectDiffRow { row_idx: usize, side: String },
    OpenAskAi,
    OpenCommit,
    CycleAgent,
    QueueAiPrompt,
    SendAiPrompt,
    ClearAiQueue,
    CloseComposer,
    RunCommit { stage_all: bool },
    SetKittyMode(KittyImageMode),
    ToggleKittyBoxes,
    AdjustKittySlider(i8),
}

#[derive(Debug, Clone)]
struct HitBox {
    rect: Rect,
    action: HitAction,
}

struct AppState {
    opts: UiOptions,
    rex_ctx: RexSessionContext,
    palette: ThemePalette,
    last_theme_check: Instant,
    last_repo_poll: Instant,
    last_repo_fingerprint: u64,

    cwd: PathBuf,
    status: Option<crate::types::StatusResponse>,
    scope: String, // "all" | "unstaged" | "staged" | "commits"
    selected_commit: Option<String>,
    commit_files: Vec<FileEntry>,
    selected_path: Option<String>,
    file_scroll: usize,

    preset: String, // "default" | "minimal" | "full"
    view_override: ViewModeOverride,
    wrap_lines: bool,
    sidebar_open: bool,
    context_lines: usize,
    expanded_folds: HashSet<String>,

    diff: Option<DiffResponse>,
    active_chunk: usize,
    v_scroll: usize,
    h_scroll: usize,
    viewport_rows: usize,
    viewport_cols: usize,

    selection: Option<SelectionTarget>,
    composer: ComposerMode,
    input_buffer: String,
    ai_queue: Vec<AiQueueItem>,
    next_queue_id: usize,
    agents: Vec<RexAgentChoice>,
    selected_agent_idx: usize,

    kitty_mode: KittyImageMode,
    kitty_slider_pct: u8,
    kitty_show_boxes: bool,
    kitty_cache: KittyPreviewCache,
    pending_kitty_rect: Option<Rect>,
    had_kitty_last_frame: bool,

    toast: Option<(String, bool, Instant)>,
    hitboxes: Vec<HitBox>,
    should_quit: bool,
    want_tui_launch: bool,
}

impl AppState {
    fn new(mut opts: UiOptions) -> Self {
        let rex_ctx = detect_session_context();
        if repo_root(&opts.cwd).is_err()
            && let Some(sid) = rex_ctx.session_id.as_deref()
            && let Some((src_bid, git_cwd)) = resolve_live_followed_cwd(
                sid,
                rex_ctx.self_block_id.as_deref(),
                opts.source_block.as_deref(),
            )
            && repo_root(&git_cwd).is_ok()
        {
            opts.cwd = git_cwd;
            opts.source_block = Some(src_bid);
        }
        let palette = load_active_rex_palette();
        let scope = opts
            .initial_scope
            .clone()
            .unwrap_or_else(|| "all".to_string());
        Self {
            cwd: opts.cwd.clone(),
            opts,
            rex_ctx,
            palette,
            last_theme_check: Instant::now(),
            last_repo_poll: Instant::now(),
            last_repo_fingerprint: 0,
            status: None,
            scope,
            selected_commit: None,
            commit_files: Vec::new(),
            selected_path: None,
            file_scroll: 0,
            preset: "default".to_string(),
            view_override: ViewModeOverride::Auto,
            wrap_lines: false,
            sidebar_open: true,
            context_lines: 3,
            expanded_folds: HashSet::new(),
            diff: None,
            active_chunk: 0,
            v_scroll: 0,
            h_scroll: 0,
            viewport_rows: 24,
            viewport_cols: 80,
            selection: None,
            composer: ComposerMode::None,
            input_buffer: String::new(),
            ai_queue: Vec::new(),
            next_queue_id: 1,
            agents: Vec::new(),
            selected_agent_idx: 0,
            kitty_mode: KittyImageMode::SideBySide,
            kitty_slider_pct: 50,
            kitty_show_boxes: true,
            kitty_cache: KittyPreviewCache::default(),
            pending_kitty_rect: None,
            had_kitty_last_frame: false,
            toast: None,
            hitboxes: Vec::new(),
            should_quit: false,
            want_tui_launch: false,
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>, is_err: bool) {
        self.toast = Some((msg.into(), is_err, Instant::now()));
    }

    fn is_split_mode(&self, diff_width: u16) -> bool {
        match self.view_override {
            ViewModeOverride::Unified => false,
            ViewModeOverride::Split => true,
            ViewModeOverride::Auto => diff_width >= 100,
        }
    }

    fn current_files(&self) -> &[FileEntry] {
        let Some(st) = &self.status else {
            return &[];
        };
        match self.scope.as_str() {
            "unstaged" => &st.unstaged,
            "staged" => &st.staged,
            "commits" => &self.commit_files,
            _ => &st.all,
        }
    }

    fn selected_file_entry(&self) -> Option<&FileEntry> {
        let sel = self.selected_path.as_deref()?;
        self.current_files().iter().find(|f| f.path == sel)
    }

    fn selected_file_index(&self) -> Option<usize> {
        let sel = self.selected_path.as_deref()?;
        self.current_files().iter().position(|f| f.path == sel)
    }

    fn refresh_agents(&mut self) {
        let prev_id = self
            .agents
            .get(self.selected_agent_idx)
            .map(|a| a.id.clone());
        self.agents = discover_rex_agents(
            self.rex_ctx.session_id.as_deref(),
            self.rex_ctx.self_block_id.as_deref(),
            self.opts.source_block.as_deref(),
            self.rex_ctx.active_window_id.as_deref(),
        );
        if let Some(pid) = prev_id
            && let Some(pos) = self.agents.iter().position(|a| a.id == pid)
        {
            self.selected_agent_idx = pos;
        } else {
            self.selected_agent_idx = 0;
        }
    }

    fn refresh_all(&mut self, advance_from_stage: bool) {
        let prev_files = self.current_files().to_vec();
        let prev_sel = self.selected_path.clone();
        let prev_idx = self.selected_file_index().unwrap_or(0);

        match load_status(&self.cwd, &self.opts.rex_home, 40) {
            Ok(st) => {
                if self.selected_commit.is_none() {
                    self.selected_commit = st.commits.first().map(|c| c.hash.clone());
                }
                self.status = Some(st);
            }
            Err(e) => {
                self.set_toast(format!("Git status error: {e}"), true);
                return;
            }
        }

        if self.scope == "commits" {
            if let Some(hash) = self.selected_commit.clone() {
                if let Ok(cf) = load_commit_files(&self.cwd, &hash) {
                    self.commit_files = cf.files;
                } else {
                    self.commit_files.clear();
                }
            } else {
                self.commit_files.clear();
            }
        }

        let init_opt = self.opts.initial_path.take();
        let files = self.current_files();
        let next_sel = if let Some(init_p) = init_opt {
            if files.iter().any(|f| f.path == init_p) {
                Some(init_p)
            } else {
                pick_next_path_after_mutation(
                    files,
                    &prev_files,
                    prev_sel.as_deref(),
                    prev_idx,
                    advance_from_stage,
                )
            }
        } else {
            pick_next_path_after_mutation(
                files,
                &prev_files,
                prev_sel.as_deref(),
                prev_idx,
                advance_from_stage,
            )
        };

        let path_changed = next_sel != prev_sel;
        self.selected_path = next_sel;
        if path_changed {
            self.expanded_folds.clear();
            self.active_chunk = 0;
            self.v_scroll = 0;
            self.h_scroll = 0;
            self.selection = None;
        }
        self.load_current_diff(path_changed);
        self.last_repo_fingerprint = compute_repo_fingerprint(&self.cwd);
    }

    fn load_current_diff(&mut self, reset_scroll: bool) {
        let Some(entry) = self.selected_file_entry().cloned() else {
            self.diff = None;
            return;
        };
        let eff_scope = if self.scope == "commits" {
            "commit"
        } else {
            self.scope.as_str()
        };
        let params = DiffParams {
            cwd: &self.cwd,
            app_home: &self.opts.rex_home,
            scope: eff_scope,
            path: &entry.path,
            old_path: entry.old_path.as_deref(),
            commit_hash: self.selected_commit.as_deref(),
            preset: &self.preset,
            context_lines: self.context_lines,
            expanded_folds: &self.expanded_folds,
        };
        match compute_diff(&params) {
            Ok(d) => {
                if self.active_chunk >= d.chunks.len() {
                    self.active_chunk = d.chunks.len().saturating_sub(1);
                }
                if reset_scroll {
                    self.v_scroll = 0;
                    self.h_scroll = 0;
                }
                self.diff = Some(d);
            }
            Err(e) => {
                self.set_toast(format!("Diff error: {e}"), true);
                self.diff = None;
            }
        }
    }

    fn select_file_by_index(&mut self, idx: usize) {
        let files = self.current_files();
        if idx >= files.len() {
            return;
        }
        let new_path = files[idx].path.clone();
        if self.selected_path.as_deref() != Some(&new_path) {
            self.selected_path = Some(new_path);
            self.expanded_folds.clear();
            self.active_chunk = 0;
            self.v_scroll = 0;
            self.h_scroll = 0;
            self.selection = None;
            self.load_current_diff(true);
        }
    }

    fn jump_to_chunk(&mut self, idx: usize) {
        let Some(d) = &self.diff else { return };
        if d.chunks.is_empty() {
            return;
        }
        let clamped = idx.min(d.chunks.len() - 1);
        self.active_chunk = clamped;
        let c = &d.chunks[clamped];
        let split = self.is_split_mode(self.viewport_cols as u16);
        let target_row = if split { c.split_row } else { c.unified_row };
        self.v_scroll = target_row.saturating_sub(1);
    }

    fn perform_stage_chunk(&mut self, idx: usize) {
        let Some(d) = &self.diff else { return };
        let Some(c) = d.chunks.get(idx).cloned() else {
            return;
        };
        if !c.patchable || self.scope == "commits" || self.scope == "staged" {
            return;
        }
        let path = d.path.clone();
        let old_path = d.old_path.clone();
        match stage_chunk(
            &self.cwd,
            &self.scope,
            &path,
            old_path.as_deref(),
            c.b_start,
            c.b_end,
            c.a_start,
            c.a_end,
            Some(&c.signature),
        ) {
            Ok(()) => {
                self.set_toast(format!("Staged hunk {}/{}", idx + 1, d.chunks.len()), false);
                self.refresh_all(false);
            }
            Err(e) => self.set_toast(format!("Stage hunk failed: {e}"), true),
        }
    }

    fn perform_unstage_chunk(&mut self, idx: usize) {
        let Some(d) = &self.diff else { return };
        let Some(c) = d.chunks.get(idx).cloned() else {
            return;
        };
        if !c.patchable || self.scope == "commits" || self.scope == "unstaged" {
            return;
        }
        let path = d.path.clone();
        let old_path = d.old_path.clone();
        match unstage_chunk(
            &self.cwd,
            &self.scope,
            &path,
            old_path.as_deref(),
            c.b_start,
            c.b_end,
            c.a_start,
            c.a_end,
            Some(&c.signature),
        ) {
            Ok(()) => {
                self.set_toast(
                    format!("Unstaged hunk {}/{}", idx + 1, d.chunks.len()),
                    false,
                );
                self.refresh_all(false);
            }
            Err(e) => self.set_toast(format!("Unstage hunk failed: {e}"), true),
        }
    }

    fn perform_revert_chunk(&mut self, idx: usize) {
        let Some(d) = &self.diff else { return };
        let Some(c) = d.chunks.get(idx).cloned() else {
            return;
        };
        if !c.patchable || self.scope == "commits" || self.scope == "staged" {
            return;
        }
        let path = d.path.clone();
        let old_path = d.old_path.clone();
        let sha = d.file_sha256.clone();
        match revert_chunk(
            &self.cwd,
            &self.opts.rex_home,
            &self.scope,
            &path,
            old_path.as_deref(),
            c.b_start,
            c.b_end,
            c.a_start,
            c.a_end,
            sha.as_deref(),
            Some(&c.signature),
        ) {
            Ok(()) => {
                self.set_toast(
                    format!("Reverted hunk {}/{} (z to undo)", idx + 1, d.chunks.len()),
                    false,
                );
                self.refresh_all(false);
            }
            Err(e) => self.set_toast(format!("Revert hunk failed: {e}"), true),
        }
    }

    fn open_ask_ai_composer(&mut self) {
        if self.selection.is_none()
            && let Some(d) = &self.diff
        {
            self.selection = Some(resolve_active_chunk_target(d, self.active_chunk));
        }
        self.refresh_agents();
        self.composer = ComposerMode::AskAi;
    }

    fn enqueue_current_ai_item(&mut self) -> bool {
        let note = self.input_buffer.trim().to_string();
        if note.is_empty() {
            self.set_toast("Type a review comment first", true);
            return false;
        }
        let Some(d) = &self.diff else {
            return false;
        };
        let target = self
            .selection
            .clone()
            .unwrap_or_else(|| resolve_active_chunk_target(d, self.active_chunk));
        let item = AiQueueItem {
            id: self.next_queue_id,
            path: target.path,
            scope: self.scope.clone(),
            commit_hash: self.selected_commit.clone(),
            side: target.side,
            breadcrumb: target.breadcrumb,
            short_label: target.short_label,
            short_ref: target.short_ref.clone(),
            line_spec: target.line_spec,
            snippet: target.snippet,
            note,
            unified_keys: target.unified_keys,
            split_keys: target.split_keys,
        };
        self.next_queue_id += 1;
        self.ai_queue.push(item);
        self.input_buffer.clear();
        self.selection = None;
        self.set_toast(
            format!("Queued review comment ({} total)", self.ai_queue.len()),
            false,
        );
        true
    }

    fn send_ai_review_now(&mut self) {
        if !self.input_buffer.trim().is_empty() {
            let _ = self.enqueue_current_ai_item();
        }
        if self.ai_queue.is_empty() {
            self.set_toast("Enter a prompt or queue at least one review item", true);
            return;
        }
        if self.agents.is_empty() {
            self.refresh_agents();
        }
        let Some(choice) = self.agents.get(self.selected_agent_idx).cloned() else {
            self.set_toast("No Rex agent target available", true);
            return;
        };
        let prompt = format_ai_review_prompt(
            &self.ai_queue,
            &self.scope,
            self.selected_commit.as_deref(),
        );
        let root = repo_root(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        match send_prompt_to_rex_agent(
            self.rex_ctx.session_id.as_deref(),
            self.opts.source_block.as_deref(),
            &choice,
            &root,
            &prompt,
        ) {
            Ok(_) => {
                let count = self.ai_queue.len();
                self.ai_queue.clear();
                self.input_buffer.clear();
                self.selection = None;
                self.composer = ComposerMode::None;
                self.set_toast(
                    format!("Sent {count} review item(s) to {}", choice.label),
                    false,
                );
            }
            Err(e) => {
                self.set_toast(format!("{e}"), true);
            }
        }
    }

    fn execute_hit_action(&mut self, action: HitAction, shift_held: bool) {
        match action {
            HitAction::SelectScope(s) => {
                if self.scope != s {
                    self.scope = s;
                    self.v_scroll = 0;
                    self.h_scroll = 0;
                    self.refresh_all(false);
                }
            }
            HitAction::SelectCommit(hash) => {
                if self.selected_commit.as_deref() != Some(&hash) {
                    self.selected_commit = Some(hash);
                    self.refresh_all(false);
                }
            }
            HitAction::SelectFile(idx) => self.select_file_by_index(idx),
            HitAction::StageFile(idx) => {
                if let Some(f) = self.current_files().get(idx).cloned() {
                    match stage_file(&self.cwd, &f.path, f.old_path.as_deref()) {
                        Ok(()) => {
                            self.set_toast(format!("Staged {}", f.path), false);
                            self.refresh_all(true);
                        }
                        Err(e) => self.set_toast(format!("Stage failed: {e}"), true),
                    }
                }
            }
            HitAction::UnstageFile(idx) => {
                if let Some(f) = self.current_files().get(idx).cloned() {
                    match unstage_file(&self.cwd, &f.path, f.old_path.as_deref()) {
                        Ok(()) => {
                            self.set_toast(format!("Unstaged {}", f.path), false);
                            self.refresh_all(false);
                        }
                        Err(e) => self.set_toast(format!("Unstage failed: {e}"), true),
                    }
                }
            }
            HitAction::DiscardFile(idx) => {
                if let Some(f) = self.current_files().get(idx).cloned() {
                    match discard_file(&self.cwd, &self.opts.rex_home, &self.scope, &f.path) {
                        Ok(()) => {
                            self.set_toast(format!("Discarded {} (z to undo)", f.path), false);
                            self.refresh_all(false);
                        }
                        Err(e) => self.set_toast(format!("Discard failed: {e}"), true),
                    }
                }
            }
            HitAction::StageAll => {
                if let Ok(root) = repo_root(&self.cwd)
                    && git_write(&root, &["add", "-A"]).is_ok()
                {
                    self.set_toast("Staged all changes", false);
                    self.refresh_all(false);
                }
            }
            HitAction::UnstageAll => {
                if let Ok(root) = repo_root(&self.cwd) {
                    let res = if crate::git::has_head(&root) {
                        git_write(&root, &["restore", "--staged", "."])
                    } else {
                        git_write(&root, &["rm", "--cached", "-r", "-f", "."])
                    };
                    if res.is_ok() {
                        self.set_toast("Unstaged all changes", false);
                        self.refresh_all(false);
                    }
                }
            }
            HitAction::UndoTop => match pop_undo(&self.cwd, &self.opts.rex_home) {
                Ok(u) => {
                    self.set_toast(format!("Restored: {}", u.label), false);
                    self.refresh_all(false);
                }
                Err(e) => self.set_toast(format!("{e}"), true),
            },
            HitAction::CyclePreset => {
                self.preset = match self.preset.as_str() {
                    "default" => "minimal".to_string(),
                    "minimal" => "full".to_string(),
                    _ => "default".to_string(),
                };
                self.load_current_diff(false);
            }
            HitAction::ToggleWrap => {
                self.wrap_lines = !self.wrap_lines;
            }
            HitAction::ToggleViewMode => {
                let currently_split = self.is_split_mode(self.viewport_cols as u16);
                self.view_override = if currently_split {
                    ViewModeOverride::Unified
                } else {
                    ViewModeOverride::Split
                };
            }
            HitAction::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
            }
            HitAction::LaunchTui => {
                self.want_tui_launch = true;
            }
            HitAction::Refresh => {
                self.refresh_all(false);
                self.set_toast("Refreshed", false);
            }
            HitAction::CloseApp => {
                self.should_quit = true;
            }
            HitAction::PrevHunk => {
                if self.active_chunk > 0 {
                    self.jump_to_chunk(self.active_chunk - 1);
                }
            }
            HitAction::NextHunk => {
                if let Some(d) = &self.diff
                    && self.active_chunk + 1 < d.chunks.len()
                {
                    self.jump_to_chunk(self.active_chunk + 1);
                }
            }
            HitAction::JumpMove(target_chunk) => {
                self.jump_to_chunk(target_chunk);
            }
            HitAction::StageActiveHunk => self.perform_stage_chunk(self.active_chunk),
            HitAction::UnstageActiveHunk => self.perform_unstage_chunk(self.active_chunk),
            HitAction::RevertActiveHunk => self.perform_revert_chunk(self.active_chunk),
            HitAction::StageHunkIdx(idx) => self.perform_stage_chunk(idx),
            HitAction::UnstageHunkIdx(idx) => self.perform_unstage_chunk(idx),
            HitAction::RevertHunkIdx(idx) => self.perform_revert_chunk(idx),
            HitAction::ToggleFileReview => {
                if let Some(d) = &self.diff {
                    let path = d.path.clone();
                    let sig = d.file_sig.clone();
                    if let Ok(reviewed) =
                        toggle_review(&self.cwd, &self.opts.rex_home, &path, None, Some(&sig))
                    {
                        self.set_toast(
                            if reviewed {
                                format!("Marked {path} reviewed ✓")
                            } else {
                                format!("Unmarked {path}")
                            },
                            false,
                        );
                        self.refresh_all(false);
                    }
                }
            }
            HitAction::ToggleChunkReview(idx) => {
                if let Some(d) = &self.diff
                    && let Some(c) = d.chunks.get(idx)
                {
                    let path = d.path.clone();
                    let sig = c.signature.clone();
                    if toggle_review(&self.cwd, &self.opts.rex_home, &path, Some(&sig), None)
                        .is_ok()
                    {
                        self.load_current_diff(false);
                    }
                }
            }
            HitAction::ExpandFold(fold_id) => {
                self.expanded_folds.insert(fold_id);
                self.load_current_diff(false);
            }
            HitAction::SelectDiffRow { row_idx, side } => {
                let Some(d) = &self.diff else { return };
                let split = self.is_split_mode(self.viewport_cols as u16);
                let anchor = if shift_held {
                    self.selection.as_ref().map(|s| s.anchor_idx)
                } else {
                    None
                };
                let start = anchor.unwrap_or(row_idx);
                if let Some(sel) =
                    resolve_selection_details(d, split, start, row_idx, &side, Some(start))
                {
                    if let Some(ci) = sel.chunk_idx {
                        self.active_chunk = ci;
                    }
                    self.selection = Some(sel);
                    if self.composer == ComposerMode::None {
                        self.open_ask_ai_composer();
                    }
                }
            }
            HitAction::OpenAskAi => {
                self.open_ask_ai_composer();
            }
            HitAction::OpenCommit => {
                self.composer = ComposerMode::Commit;
            }
            HitAction::CycleAgent => {
                self.refresh_agents();
                if !self.agents.is_empty() {
                    self.selected_agent_idx = (self.selected_agent_idx + 1) % self.agents.len();
                }
            }
            HitAction::QueueAiPrompt => {
                self.enqueue_current_ai_item();
            }
            HitAction::SendAiPrompt => {
                self.send_ai_review_now();
            }
            HitAction::ClearAiQueue => {
                self.ai_queue.clear();
                self.set_toast("Cleared queued review items", false);
            }
            HitAction::CloseComposer => {
                self.composer = ComposerMode::None;
                self.selection = None;
            }
            HitAction::RunCommit { stage_all } => {
                let msg = self.input_buffer.trim().to_string();
                match commit_changes(&self.cwd, &msg, stage_all) {
                    Ok(()) => {
                        self.input_buffer.clear();
                        self.composer = ComposerMode::None;
                        self.set_toast("Committed changes ✓", false);
                        self.refresh_all(false);
                    }
                    Err(e) => self.set_toast(format!("Commit failed: {e}"), true),
                }
            }
            HitAction::SetKittyMode(m) => {
                self.kitty_mode = m;
            }
            HitAction::ToggleKittyBoxes => {
                self.kitty_show_boxes = !self.kitty_show_boxes;
            }
            HitAction::AdjustKittySlider(delta) => {
                let next = (self.kitty_slider_pct as i16 + delta as i16).clamp(0, 100) as u8;
                self.kitty_slider_pct = next;
            }
        }
    }
}

fn compute_repo_fingerprint(cwd: &Path) -> u64 {
    let Ok(root) = repo_root(cwd) else { return 0 };
    let git_dir = root.join(".git");
    let mut acc = 0u64;
    for rel in ["index", "HEAD", "refs/heads", "FETCH_HEAD"] {
        if let Ok(meta) = fs::metadata(git_dir.join(rel))
            && let Ok(m) = meta.modified()
            && let Ok(dur) = m.duration_since(SystemTime::UNIX_EPOCH)
        {
            acc = acc
                .wrapping_mul(131)
                .wrapping_add(dur.as_nanos() as u64)
                .wrapping_add(meta.len());
        }
    }
    acc
}

pub fn run_ui(opts: UiOptions) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = AppState::new(opts);
    app.refresh_all(false);
    app.refresh_agents();

    let res = run_event_loop(&mut terminal, &mut app);

    let _ = clear_kitty_images(terminal.backend_mut());
    let _ = cleanup_scratch(&app.opts.rex_home);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if app.should_quit {
        close_self_in_rex(
            app.rex_ctx.session_id.as_deref(),
            app.rex_ctx.self_block_id.as_deref(),
            &app.opts.mode,
        );
    }

    res
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
) -> Result<()> {
    loop {
        if app.want_tui_launch {
            app.want_tui_launch = false;
            suspend_and_launch_omnidiff_tui(terminal, app)?;
        }

        let mut kitty_escape: Option<String> = None;
        terminal.draw(|f| {
            draw_app(f, app);
        })?;

        if let Some(rect) = app.pending_kitty_rect
            && let Some(d) = &app.diff
            && let Some(pm) = &d.picture_media
            && rect.width >= 16
            && rect.height >= 6
            && let Some(esc) = app.kitty_cache.prepare_escape(
                &d.file_sig,
                pm.before_abs.as_deref(),
                pm.after_abs.as_deref(),
                app.kitty_mode,
                app.kitty_slider_pct,
                app.kitty_show_boxes,
                rect.x + 1,
                rect.y + 1,
                rect.width,
                rect.height,
            )
        {
            kitty_escape = Some(esc.to_string());
        }

        if let Some(esc) = kitty_escape {
            write!(terminal.backend_mut(), "{esc}")?;
            terminal.backend_mut().flush()?;
            app.had_kitty_last_frame = true;
        } else if app.pending_kitty_rect.is_none() && app.had_kitty_last_frame {
            clear_kitty_images(terminal.backend_mut())?;
            app.kitty_cache.invalidate_sent();
            app.had_kitty_last_frame = false;
        }

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(90))? {
            match event::read()? {
                Event::Key(k) => handle_key_event(app, k),
                Event::Mouse(m) => handle_mouse_event(app, m),
                Event::Resize(_, _) => {
                    app.kitty_cache.invalidate_sent();
                }
                _ => {}
            }
        } else {
            if app.last_theme_check.elapsed() >= Duration::from_millis(1800) {
                app.last_theme_check = Instant::now();
                let (active_id, _) = detect_rex_active_theme_id();
                if active_id != app.palette.theme_id {
                    app.palette = load_active_rex_palette();
                }
            }

            if app.last_repo_poll.elapsed() >= Duration::from_millis(1600) {
                app.last_repo_poll = Instant::now();
                if app.opts.mode == "dock"
                    && let Some(sid) = app.rex_ctx.session_id.as_deref()
                    && let Some((src_bid, new_cwd)) = resolve_live_followed_cwd(
                        sid,
                        app.rex_ctx.self_block_id.as_deref(),
                        app.opts.source_block.as_deref(),
                    )
                {
                    app.opts.source_block = Some(src_bid);
                    let old_root = repo_root(&app.cwd).ok();
                    let new_root = repo_root(&new_cwd).ok();
                    if new_root.is_some() && new_root != old_root {
                        app.cwd = new_cwd;
                        app.selected_path = None;
                        app.refresh_all(false);
                        continue;
                    }
                }

                let fp = compute_repo_fingerprint(&app.cwd);
                if fp != 0 && fp != app.last_repo_fingerprint {
                    app.refresh_all(false);
                }
            }
        }
    }
    Ok(())
}

fn suspend_and_launch_omnidiff_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
) -> Result<()> {
    let eff_scope = if app.scope == "commits" {
        "commit"
    } else {
        app.scope.as_str()
    };
    let old_path = app
        .selected_file_entry()
        .and_then(|f| f.old_path.clone());
    let palette_json = serde_json::to_string(&app.palette.tui_custom).ok();

    let spec = match prepare_tui_launch(
        &app.cwd,
        &app.opts.rex_home,
        eff_scope,
        app.selected_path.as_deref(),
        old_path.as_deref(),
        app.selected_commit.as_deref(),
        &app.preset,
        palette_json.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => {
            app.set_toast(format!("TUI launch failed: {e}"), true);
            return Ok(());
        }
    };

    if spec.argv.is_empty() {
        return Ok(());
    }

    let _ = clear_kitty_images(terminal.backend_mut());
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    let mut cmd = Command::new(&spec.argv[0]);
    if spec.argv.len() > 1 {
        cmd.args(&spec.argv[1..]);
    }
    cmd.current_dir(&spec.cwd);
    let _ = cmd.status();

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()?;
    app.refresh_all(false);
    Ok(())
}

fn handle_key_event(app: &mut AppState, key: KeyEvent) {
    if app.composer != ComposerMode::None {
        match key.code {
            KeyCode::Esc => {
                app.composer = ComposerMode::None;
                app.selection = None;
            }
            KeyCode::Enter => {
                if app.composer == ComposerMode::AskAi {
                    app.send_ai_review_now();
                } else if app.composer == ComposerMode::Commit {
                    app.execute_hit_action(HitAction::RunCommit { stage_all: false }, false);
                }
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.composer == ComposerMode::AskAi {
                    app.enqueue_current_ai_item();
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.composer == ComposerMode::AskAi {
                    app.execute_hit_action(HitAction::CycleAgent, false);
                } else if app.composer == ComposerMode::Commit {
                    app.execute_hit_action(HitAction::RunCommit { stage_all: true }, false);
                }
            }
            KeyCode::Backspace => {
                app.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SUPER)
                {
                    app.input_buffer.push(c);
                }
            }
            _ => {}
        }
        return;
    }

    let is_image = app
        .diff
        .as_ref()
        .and_then(|d| d.picture_media.as_ref())
        .is_some();

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.selection.is_some() {
                app.selection = None;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('1') => app.execute_hit_action(HitAction::SelectScope("all".into()), false),
        KeyCode::Char('2') => {
            app.execute_hit_action(HitAction::SelectScope("unstaged".into()), false)
        }
        KeyCode::Char('3') => {
            app.execute_hit_action(HitAction::SelectScope("staged".into()), false)
        }
        KeyCode::Char('4') => {
            app.execute_hit_action(HitAction::SelectScope("commits".into()), false)
        }
        KeyCode::Char('j') => {
            if let Some(idx) = app.selected_file_index() {
                let next = (idx + 1).min(app.current_files().len().saturating_sub(1));
                app.select_file_by_index(next);
            } else if !app.current_files().is_empty() {
                app.select_file_by_index(0);
            }
        }
        KeyCode::Char('k') => {
            if let Some(idx) = app.selected_file_index() {
                app.select_file_by_index(idx.saturating_sub(1));
            }
        }
        KeyCode::Char('J') | KeyCode::Down => {
            app.v_scroll = app.v_scroll.saturating_add(3);
        }
        KeyCode::Char('K') | KeyCode::Up => {
            app.v_scroll = app.v_scroll.saturating_sub(3);
        }
        KeyCode::PageDown | KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::PageDown =>
        {
            app.v_scroll = app
                .v_scroll
                .saturating_add(app.viewport_rows.saturating_sub(4).max(1));
        }
        KeyCode::PageUp | KeyCode::Char('u')
            if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::PageUp =>
        {
            app.v_scroll = app
                .v_scroll
                .saturating_sub(app.viewport_rows.saturating_sub(4).max(1));
        }
        KeyCode::Char('g') => {
            app.v_scroll = 0;
        }
        KeyCode::Char('G') => {
            app.v_scroll = usize::MAX / 2;
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if is_image {
                app.execute_hit_action(HitAction::AdjustKittySlider(-5), false);
            } else if !app.wrap_lines {
                app.h_scroll = app.h_scroll.saturating_sub(6);
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if is_image {
                app.execute_hit_action(HitAction::AdjustKittySlider(5), false);
            } else if !app.wrap_lines {
                app.h_scroll = app.h_scroll.saturating_add(6);
            }
        }
        KeyCode::Char('0') => {
            app.h_scroll = 0;
        }
        KeyCode::Char('$') => {
            if let Some(d) = &app.diff {
                let split = app.is_split_mode(app.viewport_cols as u16);
                let max_c = if split {
                    d.max_split_chars
                } else {
                    d.max_unified_chars
                };
                app.h_scroll = max_c.saturating_sub(40);
            }
        }
        KeyCode::Char('n') | KeyCode::Char(']') => {
            app.execute_hit_action(HitAction::NextHunk, false);
        }
        KeyCode::Char('p') | KeyCode::Char('[') => {
            app.execute_hit_action(HitAction::PrevHunk, false);
        }
        KeyCode::Char('s') => {
            app.execute_hit_action(HitAction::StageActiveHunk, false);
        }
        KeyCode::Char('u') => {
            app.execute_hit_action(HitAction::UnstageActiveHunk, false);
        }
        KeyCode::Char('x') => {
            if is_image {
                app.execute_hit_action(HitAction::ToggleKittyBoxes, false);
            } else {
                app.execute_hit_action(HitAction::RevertActiveHunk, false);
            }
        }
        KeyCode::Char(' ') => {
            if let Some(idx) = app.selected_file_index() {
                if app.scope == "staged" {
                    app.execute_hit_action(HitAction::UnstageFile(idx), false);
                } else {
                    app.execute_hit_action(HitAction::StageFile(idx), false);
                }
            }
        }
        KeyCode::Char('S') => {
            app.execute_hit_action(HitAction::StageAll, false);
        }
        KeyCode::Char('U') => {
            app.execute_hit_action(HitAction::UnstageAll, false);
        }
        KeyCode::Char('v') => {
            app.execute_hit_action(HitAction::ToggleFileReview, false);
        }
        KeyCode::Char('z') => {
            app.execute_hit_action(HitAction::UndoTop, false);
        }
        KeyCode::Char('m') => {
            if let Some(d) = &app.diff
                && let Some(c) = d.chunks.get(app.active_chunk)
                && let Some(mi) = &c.move_info
                && let Some(tc) = mi.target_chunk
            {
                app.jump_to_chunk(tc);
            }
        }
        KeyCode::Char('i') => {
            if is_image {
                app.kitty_mode = app.kitty_mode.next();
            }
        }
        KeyCode::Tab => {
            app.execute_hit_action(HitAction::ToggleViewMode, false);
        }
        KeyCode::Char('w') => {
            app.execute_hit_action(HitAction::ToggleWrap, false);
        }
        KeyCode::Char('b') => {
            app.execute_hit_action(HitAction::ToggleSidebar, false);
        }
        KeyCode::Char('F') => {
            app.execute_hit_action(HitAction::CyclePreset, false);
        }
        KeyCode::Char('a') => {
            app.execute_hit_action(HitAction::OpenAskAi, false);
        }
        KeyCode::Char('c') => {
            app.execute_hit_action(HitAction::OpenCommit, false);
        }
        KeyCode::Char('t') => {
            app.execute_hit_action(HitAction::LaunchTui, false);
        }
        KeyCode::Char('r') => {
            app.execute_hit_action(HitAction::Refresh, false);
        }
        _ => {}
    }
}

fn handle_mouse_event(app: &mut AppState, mouse: MouseEvent) {
    let col = mouse.column;
    let row = mouse.row;
    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            for hb in app.hitboxes.iter().rev() {
                if col >= hb.rect.x
                    && col < hb.rect.x + hb.rect.width
                    && row >= hb.rect.y
                    && row < hb.rect.y + hb.rect.height
                {
                    let act = hb.action.clone();
                    app.execute_hit_action(act, shift);
                    return;
                }
            }
        }
        MouseEventKind::ScrollDown => {
            if app.sidebar_open && col < 36 {
                let max_f = app.current_files().len().saturating_sub(1);
                app.file_scroll = (app.file_scroll + 2).min(max_f);
            } else if shift {
                app.h_scroll = app.h_scroll.saturating_add(6);
            } else {
                app.v_scroll = app.v_scroll.saturating_add(3);
            }
        }
        MouseEventKind::ScrollUp => {
            if app.sidebar_open && col < 36 {
                app.file_scroll = app.file_scroll.saturating_sub(2);
            } else if shift {
                app.h_scroll = app.h_scroll.saturating_sub(6);
            } else {
                app.v_scroll = app.v_scroll.saturating_sub(3);
            }
        }
        MouseEventKind::ScrollRight => {
            app.h_scroll = app.h_scroll.saturating_add(6);
        }
        MouseEventKind::ScrollLeft => {
            app.h_scroll = app.h_scroll.saturating_sub(6);
        }
        _ => {}
    }
}

fn push_button(
    spans: &mut Vec<Span<'static>>,
    hitboxes: &mut Vec<HitBox>,
    row_y: u16,
    cur_x: &mut u16,
    label: String,
    style: Style,
    action: HitAction,
) {
    let w = label.chars().count() as u16;
    hitboxes.push(HitBox {
        rect: Rect {
            x: *cur_x,
            y: row_y,
            width: w,
            height: 1,
        },
        action,
    });
    spans.push(Span::styled(label, style));
    *cur_x += w;
}

fn push_sep(spans: &mut Vec<Span<'static>>, cur_x: &mut u16, text: &'static str, style: Style) {
    let w = text.chars().count() as u16;
    spans.push(Span::styled(text, style));
    *cur_x += w;
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, cur_w: u16, target_w: u16, bg: Color) {
    if cur_w < target_w {
        spans.push(Span::styled(
            " ".repeat((target_w - cur_w) as usize),
            Style::default().bg(bg),
        ));
    }
}


fn draw_app(f: &mut Frame<'_>, app: &mut AppState) {
    app.hitboxes.clear();
    app.pending_kitty_rect = None;

    let full = f.area();
    let pal = app.palette.clone();
    f.render_widget(Block::default().style(Style::default().bg(pal.bg)), full);

    let bottom_h = if app.composer != ComposerMode::None {
        3
    } else {
        1
    };

    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Clean top bar + subtle divider
            Constraint::Min(6),
            Constraint::Length(bottom_h),
        ])
        .split(full);

    let body = vert[1];
    let show_sidebar = app.sidebar_open && body.width >= 76;
    let side_w = if show_sidebar {
        if body.width >= 136 { 34 } else { 28 }
    } else {
        0
    };

    draw_header_bar(f, app, vert[0], side_w);

    if show_sidebar {
        let horiz = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(side_w), Constraint::Min(30)])
            .split(body);
        draw_sidebar(f, app, horiz[0]);
        draw_diff_pane(f, app, horiz[1]);
    } else {
        draw_diff_pane(f, app, body);
    }

    draw_bottom_bar(f, app, vert[2]);
}

fn draw_header_bar(f: &mut Frame<'_>, app: &mut AppState, area: Rect, side_w: u16) {
    let pal = &app.palette;
    let r0 = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Block::default().style(Style::default().bg(pal.chrome)),
        r0,
    );

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = r0.x;
    let y = r0.y;

    push_sep(&mut spans, &mut x, "  ", Style::default().bg(pal.chrome));
    push_button(
        &mut spans,
        &mut app.hitboxes,
        y,
        &mut x,
        "◆ OmniDiff".to_string(),
        Style::default()
            .fg(pal.ask_text)
            .bg(pal.chrome)
            .add_modifier(Modifier::BOLD),
        HitAction::ToggleSidebar,
    );

    if let Some(st) = &app.status {
        let repo_name = PathBuf::from(&st.root)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        let branch = st.branch.as_deref().unwrap_or("HEAD");
        let repo_txt = format!("   {repo_name}:{branch}    ");
        let w = repo_txt.chars().count() as u16;
        spans.push(Span::styled(
            repo_txt,
            Style::default().fg(pal.muted).bg(pal.chrome),
        ));
        x += w;

        for (key, num_key, lbl, cnt) in [
            ("all", "1", "All", st.all.len()),
            ("unstaged", "2", "Unstaged", st.unstaged.len()),
            ("staged", "3", "Staged", st.staged.len()),
            ("commits", "4", "Commits", st.commits.len()),
        ] {
            let active = app.scope == key;
            let text = format!(" {num_key} {lbl} {cnt} ");
            let style = if active {
                Style::default()
                    .fg(Color::Rgb(12, 16, 26))
                    .bg(pal.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(pal.muted).bg(pal.chrome)
            };
            push_button(
                &mut spans,
                &mut app.hitboxes,
                y,
                &mut x,
                text,
                style,
                HitAction::SelectScope(key.to_string()),
            );
            push_sep(&mut spans, &mut x, "  ", Style::default().bg(pal.chrome));
        }

        if st.undo_count > 0 {
            push_button(
                &mut spans,
                &mut app.hitboxes,
                y,
                &mut x,
                format!(" ↶ Undo ({}) ", st.undo_count),
                Style::default().fg(pal.warn_text).bg(pal.chrome),
                HitAction::UndoTop,
            );
        }
    }

    // Minimal right-hand controls: only Split/Unified toggle + Close
    let split_active = app.is_split_mode(app.viewport_cols as u16);
    let view_lbl = if split_active {
        " Split (Tab) "
    } else {
        " Unified (Tab) "
    };
    let close_lbl = " × ";
    let right_w = (view_lbl.chars().count() + 2 + close_lbl.chars().count() + 2) as u16;

    let used_w = x.saturating_sub(r0.x);
    if used_w + right_w < r0.width {
        let pad = r0.width - used_w - right_w;
        spans.push(Span::styled(
            " ".repeat(pad as usize),
            Style::default().bg(pal.chrome),
        ));
        x += pad;
    }

    push_button(
        &mut spans,
        &mut app.hitboxes,
        y,
        &mut x,
        view_lbl.to_string(),
        Style::default().fg(pal.muted).bg(pal.surface),
        HitAction::ToggleViewMode,
    );
    push_sep(&mut spans, &mut x, "  ", Style::default().bg(pal.chrome));
    push_button(
        &mut spans,
        &mut app.hitboxes,
        y,
        &mut x,
        close_lbl.to_string(),
        Style::default().fg(pal.muted).bg(pal.chrome),
        HitAction::CloseApp,
    );

    f.render_widget(Paragraph::new(Line::from(spans)), r0);

    if area.height >= 2 {
        let r1 = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };
        let rule_str = if side_w > 0 && side_w < area.width {
            let left_len = (side_w.saturating_sub(1)) as usize;
            let right_len = (area.width.saturating_sub(side_w)) as usize;
            format!("{}┬{}", "─".repeat(left_len), "─".repeat(right_len))
        } else {
            "─".repeat(area.width as usize)
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                rule_str,
                Style::default().fg(pal.border).bg(pal.bg),
            )),
            r1,
        );
    }
}

fn draw_sidebar(f: &mut Frame<'_>, app: &mut AppState, area: Rect) {
    let pal = &app.palette;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(pal.border))
        .style(Style::default().bg(pal.surface));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 4 || inner.width < 16 {
        return;
    }

    // Leave row 0 blank for top breathing space; row 1 is a quiet section label
    let hdr_rect = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    let file_count = app.current_files().len();
    let sec_title = if app.scope == "commits" {
        "  COMMITS & FILES".to_string()
    } else {
        format!("  FILES ({file_count})")
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            sec_title,
            Style::default()
                .fg(pal.dim)
                .bg(pal.surface)
                .add_modifier(Modifier::BOLD),
        )),
        hdr_rect,
    );

    // Leave row 2 blank below the section label
    let mut list_top = inner.y + 3;
    let mut list_h = inner.height.saturating_sub(3);

    if app.scope == "commits"
        && let Some(st) = &app.status
        && !st.commits.is_empty()
    {
        let ch = (list_h / 3).clamp(3, 7);
        for (i, c) in st.commits.iter().take(ch as usize).enumerate() {
            let cy = list_top + i as u16;
            let row_rect = Rect {
                x: inner.x,
                y: cy,
                width: inner.width,
                height: 1,
            };
            let is_sel = app.selected_commit.as_deref() == Some(&c.hash);
            let bg = if is_sel {
                pal.selected_file_bg
            } else {
                pal.surface
            };
            let bar = if is_sel { "▌ " } else { "  " };
            let max_subj = (inner.width as usize).saturating_sub(12);
            let subj: String = c.subject.chars().take(max_subj).collect();
            let line = Line::from(vec![
                Span::styled(bar, Style::default().fg(pal.ask_text).bg(bg)),
                Span::styled(
                    format!("{} ", c.short),
                    Style::default()
                        .fg(pal.ask_text)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(subj, Style::default().fg(pal.fg).bg(bg)),
            ]);
            f.render_widget(
                Paragraph::new(line).style(Style::default().bg(bg)),
                row_rect,
            );
            app.hitboxes.push(HitBox {
                rect: row_rect,
                action: HitAction::SelectCommit(c.hash.clone()),
            });
        }
        list_top += ch + 1;
        list_h = list_h.saturating_sub(ch + 1);
    }

    let files = app.current_files().to_vec();
    if files.is_empty() {
        let empty_rect = Rect {
            x: inner.x,
            y: list_top,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                "  ✓ Working tree clean",
                Style::default().fg(pal.add_text),
            )),
            empty_rect,
        );
        return;
    }

    if let Some(sel_idx) = app.selected_file_index() {
        if sel_idx < app.file_scroll {
            app.file_scroll = sel_idx;
        } else if sel_idx >= app.file_scroll + (list_h as usize).max(1) {
            app.file_scroll = sel_idx.saturating_sub((list_h as usize).saturating_sub(1));
        }
    }

    for (vis_i, (idx, file)) in files
        .iter()
        .enumerate()
        .skip(app.file_scroll)
        .take(list_h as usize)
        .enumerate()
    {
        let ry = list_top + vis_i as u16;
        let row_rect = Rect {
            x: inner.x,
            y: ry,
            width: inner.width,
            height: 1,
        };
        let selected = app.selected_path.as_deref() == Some(&file.path);
        let bg = if selected {
            pal.selected_file_bg
        } else {
            pal.surface
        };

        let status_fg = match file.status.as_str() {
            "A" => pal.add_text,
            "D" => pal.del_text,
            "R" | "C" => pal.move_text,
            "?" => pal.warn_text,
            "U" => pal.warn_text,
            _ => pal.upd_text,
        };

        let fname = file.path.rsplit('/').next().unwrap_or(&file.path);
        let sel_bar = if selected { "▌ " } else { "  " };
        let rev_mark = if file.reviewed { "✓" } else { " " };

        let add_s = format!("+{}", file.add);
        let del_s = format!("-{}", file.del);
        let stat_len = if file.binary {
            5
        } else {
            add_s.len() + 1 + del_s.len() + 2
        };

        let avail_name = (inner.width as usize)
            .saturating_sub(6 + stat_len)
            .max(6);
        let disp_name: String = fname.chars().take(avail_name).collect();

        let mut spans = vec![
            Span::styled(
                sel_bar,
                Style::default()
                    .fg(pal.ask_text)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}{rev_mark}", file.status),
                Style::default()
                    .fg(if file.reviewed {
                        pal.add_text
                    } else {
                        status_fg
                    })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {:<width$} ", disp_name, width = avail_name),
                Style::default()
                    .fg(if selected { pal.fg } else { pal.muted })
                    .bg(bg)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];

        if file.binary {
            spans.push(Span::styled(
                "BIN  ",
                Style::default().fg(pal.warn_text).bg(bg),
            ));
        } else {
            spans.push(Span::styled(
                format!("{add_s} "),
                Style::default().fg(pal.add_text).bg(bg),
            ));
            spans.push(Span::styled(
                format!("{del_s}  "),
                Style::default().fg(pal.del_text).bg(bg),
            ));
        }

        app.hitboxes.push(HitBox {
            rect: row_rect,
            action: HitAction::SelectFile(idx),
        });

        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
            row_rect,
        );
    }
}

fn draw_diff_pane(f: &mut Frame<'_>, app: &mut AppState, area: Rect) {
    let pal = app.palette.clone();
    if area.height < 5 || area.width < 20 {
        return;
    }

    let Some(diff) = app.diff.clone() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "   Select a changed file on the left to inspect its diff.",
                Style::default().fg(pal.muted),
            )),
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            },
        );
        return;
    };

    // 2-row calm file bar + 1 blank breathing row above the code
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .split(area);

    let split_active = app.is_split_mode(area.width);
    draw_sticky_hunk_toolbar(f, app, &diff, layout[0], split_active);

    // Draw center split divider on the 1-row top breathing gap if in split mode
    if split_active && !diff.binary && diff.picture_media.is_none() {
        let gap = layout[1];
        let left_w = (gap.width as usize).saturating_sub(1) / 2;
        let line = Line::from(vec![
            Span::styled(" ".repeat(left_w), Style::default().bg(pal.bg)),
            Span::styled("│", Style::default().fg(pal.border).bg(pal.bg)),
        ]);
        f.render_widget(Paragraph::new(line), gap);
    }

    let vp = layout[2];
    app.viewport_rows = vp.height as usize;
    app.viewport_cols = vp.width as usize;

    if diff.picture_media.is_some() {
        draw_picture_diff_card(f, app, &diff, vp);
    } else if diff.binary {
        draw_binary_card(f, app, &diff, vp);
    } else if split_active {
        draw_split_code_diff(f, app, &diff, vp);
    } else {
        draw_unified_code_diff(f, app, &diff, vp);
    }
}

fn draw_sticky_hunk_toolbar(
    f: &mut Frame<'_>,
    app: &mut AppState,
    diff: &DiffResponse,
    area: Rect,
    split_active: bool,
) {
    let pal = &app.palette;
    let r0 = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Block::default().style(Style::default().bg(pal.bg)),
        r0,
    );

    let mut s0: Vec<Span<'static>> = Vec::new();
    let mut x0 = r0.x;

    let path_disp = if let Some(old) = &diff.old_path {
        format!("   {old} → {}  ", diff.path)
    } else {
        format!("   {}  ", diff.path)
    };
    let pw = path_disp.chars().count() as u16;
    s0.push(Span::styled(
        path_disp,
        Style::default()
            .fg(pal.fg)
            .bg(pal.bg)
            .add_modifier(Modifier::BOLD),
    ));
    x0 += pw;

    let c = &diff.counts;
    let add_txt = format!("+{} ", c.insertions);
    let del_txt = format!("-{}   ", c.deletions);
    s0.push(Span::styled(
        add_txt.clone(),
        Style::default()
            .fg(pal.add_text)
            .bg(pal.bg)
            .add_modifier(Modifier::BOLD),
    ));
    x0 += add_txt.chars().count() as u16;
    s0.push(Span::styled(
        del_txt.clone(),
        Style::default()
            .fg(pal.del_text)
            .bg(pal.bg)
            .add_modifier(Modifier::BOLD),
    ));
    x0 += del_txt.chars().count() as u16;

    let total_chunks = diff.chunks.len();
    let cur_hunk = if total_chunks == 0 {
        0
    } else {
        app.active_chunk.min(total_chunks - 1) + 1
    };

    if let Some(chunk) = diff.chunks.get(app.active_chunk)
        && let Some(bc) = &chunk.breadcrumb
    {
        let bc_trunc: String = bc.chars().take(42).collect();
        let bt = format!("@ {bc_trunc}   ");
        let bw = bt.chars().count() as u16;
        s0.push(Span::styled(bt, Style::default().fg(pal.dim).bg(pal.bg)));
        x0 += bw;
    }

    // Right-align calm hunk counter + 3 understated actions
    let hunk_lbl = format!(" Hunk {cur_hunk}/{total_chunks} ");
    let stage_lbl = if app.scope == "staged" {
        " u unstage "
    } else {
        " s stage "
    };
    let rev_lbl = if diff.file_reviewed {
        " ✓ reviewed "
    } else {
        " v review "
    };
    let ai_lbl = " a ask AI ";
    let right_w = (hunk_lbl.chars().count()
        + 4
        + stage_lbl.chars().count()
        + 2
        + rev_lbl.chars().count()
        + 2
        + ai_lbl.chars().count()
        + 2) as u16;

    let used_w = x0.saturating_sub(r0.x);
    if used_w + right_w < r0.width {
        let pad = r0.width - used_w - right_w;
        s0.push(Span::styled(
            " ".repeat(pad as usize),
            Style::default().bg(pal.bg),
        ));
        x0 += pad;
    }

    push_button(
        &mut s0,
        &mut app.hitboxes,
        r0.y,
        &mut x0,
        "◄".to_string(),
        Style::default().fg(pal.muted).bg(pal.bg),
        HitAction::PrevHunk,
    );
    let hw = hunk_lbl.chars().count() as u16;
    s0.push(Span::styled(
        hunk_lbl,
        Style::default()
            .fg(pal.ask_text)
            .bg(pal.bg)
            .add_modifier(Modifier::BOLD),
    ));
    x0 += hw;
    push_button(
        &mut s0,
        &mut app.hitboxes,
        r0.y,
        &mut x0,
        "►".to_string(),
        Style::default().fg(pal.muted).bg(pal.bg),
        HitAction::NextHunk,
    );
    push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.bg));

    if app.scope != "commits" && total_chunks > 0 {
        let act = if app.scope == "staged" {
            HitAction::UnstageActiveHunk
        } else {
            HitAction::StageActiveHunk
        };
        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            stage_lbl.to_string(),
            Style::default().fg(pal.add_text).bg(pal.surface),
            act,
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.bg));
    }

    push_button(
        &mut s0,
        &mut app.hitboxes,
        r0.y,
        &mut x0,
        rev_lbl.to_string(),
        Style::default()
            .fg(if diff.file_reviewed {
                pal.add_text
            } else {
                pal.muted
            })
            .bg(pal.surface),
        HitAction::ToggleFileReview,
    );
    push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.bg));

    push_button(
        &mut s0,
        &mut app.hitboxes,
        r0.y,
        &mut x0,
        ai_lbl.to_string(),
        Style::default().fg(pal.ask_text).bg(pal.surface),
        HitAction::OpenAskAi,
    );

    f.render_widget(Paragraph::new(Line::from(s0)), r0);

    // Row 1: Subtle divider rule under the file bar
    let r1 = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    let total_w = area.width as usize;
    if split_active && !diff.binary && diff.picture_media.is_none() {
        let left_w = total_w.saturating_sub(1) / 2;
        let right_w = total_w.saturating_sub(1).saturating_sub(left_w);
        let left_lbl = "─── before ";
        let right_lbl = "─── after ";
        let l_pad = left_w.saturating_sub(left_lbl.chars().count());
        let r_pad = right_w.saturating_sub(right_lbl.chars().count());
        let line = Line::from(vec![
            Span::styled(left_lbl, Style::default().fg(pal.dim).bg(pal.bg)),
            Span::styled(
                "─".repeat(l_pad),
                Style::default().fg(pal.border).bg(pal.bg),
            ),
            Span::styled("┬", Style::default().fg(pal.border).bg(pal.bg)),
            Span::styled(right_lbl, Style::default().fg(pal.dim).bg(pal.bg)),
            Span::styled(
                "─".repeat(r_pad),
                Style::default().fg(pal.border).bg(pal.bg),
            ),
        ]);
        f.render_widget(Paragraph::new(line), r1);
    } else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "─".repeat(total_w),
                Style::default().fg(pal.border).bg(pal.bg),
            )),
            r1,
        );
    }
}

fn draw_picture_diff_card(
    f: &mut Frame<'_>,
    app: &mut AppState,
    diff: &DiffResponse,
    area: Rect,
) {
    let pal = &app.palette;
    let Some(pm) = &diff.picture_media else {
        return;
    };

    let stats = app
        .kitty_cache
        .ensure_loaded(
            &diff.file_sig,
            pm.before_abs.as_deref(),
            pm.after_abs.as_deref(),
        )
        .clone();

    let bar_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let mut bar_spans = Vec::new();
    let mut bx = bar_rect.x;

    for mode in [
        KittyImageMode::SideBySide,
        KittyImageMode::DiffHeatmap,
        KittyImageMode::SplitWipe,
        KittyImageMode::OnionBlend,
    ] {
        let active = app.kitty_mode == mode;
        let style = if active {
            Style::default()
                .fg(Color::Rgb(12, 16, 26))
                .bg(pal.ask_text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(pal.muted).bg(pal.raised)
        };
        push_button(
            &mut bar_spans,
            &mut app.hitboxes,
            bar_rect.y,
            &mut bx,
            format!(" {} ", mode.label()),
            style,
            HitAction::SetKittyMode(mode),
        );
        push_sep(&mut bar_spans, &mut bx, " ", Style::default().bg(pal.bg));
    }

    push_button(
        &mut bar_spans,
        &mut app.hitboxes,
        bar_rect.y,
        &mut bx,
        format!(
            " Boxes:{} (x) ",
            if app.kitty_show_boxes { "ON" } else { "OFF" }
        ),
        Style::default().fg(pal.ask_text).bg(pal.raised),
        HitAction::ToggleKittyBoxes,
    );
    push_sep(&mut bar_spans, &mut bx, " ", Style::default().bg(pal.bg));

    if matches!(
        app.kitty_mode,
        KittyImageMode::SplitWipe | KittyImageMode::OnionBlend
    ) {
        push_button(
            &mut bar_spans,
            &mut app.hitboxes,
            bar_rect.y,
            &mut bx,
            " ◄ ".to_string(),
            Style::default().fg(pal.fg).bg(pal.raised),
            HitAction::AdjustKittySlider(-10),
        );
        let pct_s = format!(" {}% ", app.kitty_slider_pct);
        let pw = pct_s.chars().count() as u16;
        bar_spans.push(Span::styled(
            pct_s,
            Style::default()
                .fg(pal.ask_text)
                .bg(pal.surface)
                .add_modifier(Modifier::BOLD),
        ));
        bx += pw;
        push_button(
            &mut bar_spans,
            &mut app.hitboxes,
            bar_rect.y,
            &mut bx,
            " ► ".to_string(),
            Style::default().fg(pal.fg).bg(pal.raised),
            HitAction::AdjustKittySlider(10),
        );
        push_sep(&mut bar_spans, &mut bx, " ", Style::default().bg(pal.bg));
    }

    let b_dim = stats
        .before_dims
        .map(|(w, h)| format!("{w}×{h}"))
        .unwrap_or_else(|| "—".to_string());
    let a_dim = stats
        .after_dims
        .map(|(w, h)| format!("{w}×{h}"))
        .unwrap_or_else(|| "—".to_string());
    let pct = if stats.total_pixels > 0 {
        (stats.changed_pixels as f64 / stats.total_pixels as f64) * 100.0
    } else {
        0.0
    };
    let meta = format!(
        "  Before: {b_dim}   After: {a_dim}   Changed: {} px ({pct:.2}%)  [Kitty Graphics Protocol]",
        stats.changed_pixels
    );
    bar_spans.push(Span::styled(meta, Style::default().fg(pal.muted)));

    f.render_widget(Paragraph::new(Line::from(bar_spans)), bar_rect);

    if area.height > 2 {
        let canvas = Rect {
            x: area.x + 1,
            y: area.y + 2,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pal.border))
            .title(Span::styled(
                format!(" Kitty Image Diff · {} ", app.kitty_mode.label()),
                Style::default().fg(pal.ask_text),
            ));
        let inner = block.inner(canvas);
        f.render_widget(block, canvas);
        app.pending_kitty_rect = Some(inner);
    }
}

fn draw_binary_card(f: &mut Frame<'_>, app: &AppState, diff: &DiffResponse, area: Rect) {
    let pal = &app.palette;
    let mut lines = vec![
        Line::from(Span::styled(
            "   Binary / Non-Text File Changed",
            Style::default()
                .fg(pal.warn_text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if let Some(content) = &diff.content {
        let pretty = serde_json::to_string_pretty(content).unwrap_or_default();
        for l in pretty.lines().take(24) {
            lines.push(Line::from(Span::styled(
                format!("   {l}"),
                Style::default().fg(pal.ask_text),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn render_elide_row(
    f: &mut Frame<'_>,
    app: &mut AppState,
    fold_id: Option<&str>,
    count: usize,
    row_rect: Rect,
) {
    let pal = &app.palette;
    let line = Line::from(vec![
        Span::styled(
            "       ⋯       │  ",
            Style::default().fg(pal.border).bg(pal.bg),
        ),
        Span::styled(
            format!("↕ {count} unchanged lines"),
            Style::default().fg(pal.fold_fg).bg(pal.bg),
        ),
    ]);
    f.render_widget(Paragraph::new(line), row_rect);
    if let Some(fid) = fold_id.filter(|s| !s.is_empty()) {
        app.hitboxes.push(HitBox {
            rect: row_rect,
            action: HitAction::ExpandFold(fid.to_string()),
        });
    }
}

fn has_intraline_token_highlights(segs: &[TextSegment], row_bg_tag: Option<&str>) -> bool {
    let non_ws: Vec<&TextSegment> = segs.iter().filter(|s| !s.t.trim().is_empty()).collect();
    if non_ws.is_empty() {
        return false;
    }
    if non_ws.iter().any(|s| s.op.as_deref() == Some("u")) {
        return true;
    }
    let has_plain = non_ws.iter().any(|s| s.op.is_none());
    let has_op = non_ws
        .iter()
        .any(|s| matches!(s.op.as_deref(), Some("i") | Some("d") | Some("u")));
    if has_plain && has_op {
        return true;
    }
    if matches!(row_bg_tag, Some("i") | Some("d")) {
        return false;
    }
    has_op
}

fn style_code_segments(
    segs: &[TextSegment],
    row_bg_tag: Option<&str>,
    row_bg: Color,
    pal: &ThemePalette,
    h_scroll: usize,
    max_cols: usize,
) -> Vec<Span<'static>> {
    let mut state = LineSyntaxState::default();
    let mut raw_tokens: Vec<(String, Color, Color, bool)> = Vec::new();
    let paint_intraline = has_intraline_token_highlights(segs, row_bg_tag);

    for seg in segs {
        let (seg_bg, on_hi, bold) = if paint_intraline {
            match seg.op.as_deref() {
                Some("i") => (pal.seg_i, true, true),
                Some("d") => (pal.seg_d, true, true),
                Some("u") => {
                    let b = match row_bg_tag {
                        Some("d") => pal.seg_d,
                        Some("i") => pal.seg_i,
                        _ => pal.seg_u,
                    };
                    (b, true, true)
                }
                _ => (row_bg, false, false),
            }
        } else {
            (row_bg, false, false)
        };
        for (tok_str, fg) in tokenize_syntax_segment(&seg.t, pal, &mut state, on_hi) {
            raw_tokens.push((tok_str, fg, seg_bg, bold));
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut col_idx = 0usize;
    let end_col = h_scroll + max_cols;

    for (tok, fg, bg, bold) in raw_tokens {
        let tok_chars: Vec<char> = tok.chars().collect();
        let tok_len = tok_chars.len();
        if col_idx + tok_len <= h_scroll {
            col_idx += tok_len;
            continue;
        }
        if col_idx >= end_col {
            break;
        }
        let s = h_scroll.saturating_sub(col_idx);
        let e = (end_col - col_idx).min(tok_len);
        if s < e {
            let slice: String = tok_chars[s..e].iter().collect();
            let mut st = Style::default().fg(fg).bg(bg);
            if bold {
                st = st.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(slice, st));
        }
        col_idx += tok_len;
    }

    if col_idx < end_col {
        let pad = end_col - col_idx.max(h_scroll);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));
        }
    }

    spans
}

fn draw_unified_code_diff(
    f: &mut Frame<'_>,
    app: &mut AppState,
    diff: &DiffResponse,
    area: Rect,
) {
    let pal = app.palette.clone();
    let rows = &diff.unified_rows;
    if rows.is_empty() {
        return;
    }

    let max_scroll = rows.len().saturating_sub(area.height as usize);
    if app.v_scroll > max_scroll {
        app.v_scroll = max_scroll;
    }

    let h_scroll = if app.wrap_lines { 0 } else { app.h_scroll };

    for (vis_i, (row_idx, r)) in rows
        .iter()
        .enumerate()
        .skip(app.v_scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let ry = area.y + vis_i as u16;
        let row_rect = Rect {
            x: area.x,
            y: ry,
            width: area.width,
            height: 1,
        };

        if r.kind == "spacer" || r.kind == "chunk" {
            continue;
        }

        if r.kind == "elide" {
            render_elide_row(
                f,
                app,
                r.fold_id.as_deref(),
                r.count.unwrap_or(0),
                row_rect,
            );
            continue;
        }

        let in_chunk = r.chunk_idx.is_some();
        let in_active_chunk = r.chunk_idx == Some(app.active_chunk);
        let is_selected = app
            .selection
            .as_ref()
            .is_some_and(|s| s.unified_keys.contains(&r.key));
        let is_queued = app
            .ai_queue
            .iter()
            .any(|q| q.path == diff.path && q.unified_keys.contains(&r.key));

        let base_bg = if is_selected {
            pal.ask_row_bg
        } else if is_queued {
            pal.queued_row_bg
        } else {
            match r.row_bg.as_deref() {
                Some("i") => pal.row_i,
                Some("d") => pal.row_d,
                Some("u") => pal.row_u,
                Some("m") => pal.row_m,
                _ => pal.bg,
            }
        };

        let left_bar = if is_selected {
            Span::styled("▌ ", Style::default().fg(pal.ask_text).bg(base_bg))
        } else if is_queued {
            Span::styled("▌ ", Style::default().fg(pal.move_text).bg(base_bg))
        } else if in_active_chunk {
            Span::styled("▌ ", Style::default().fg(pal.chunk_active_rail).bg(base_bg))
        } else if in_chunk {
            Span::styled("▏ ", Style::default().fg(pal.chunk_rail).bg(base_bg))
        } else {
            Span::styled("  ", Style::default().bg(base_bg))
        };

        let old_s = r
            .old_no
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".into());
        let new_s = r
            .new_no
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".into());
        let marker = r.marker.as_deref().unwrap_or(" ");
        let marker_fg = match marker {
            "+" => pal.add_text,
            "-" => pal.del_text,
            _ => pal.line_num,
        };
        let num_fg = if in_chunk {
            pal.line_num_active
        } else {
            pal.line_num
        };

        let mut spans = vec![
            left_bar,
            Span::styled(
                format!("{old_s} {new_s}  "),
                Style::default().fg(num_fg).bg(base_bg),
            ),
            Span::styled(
                marker.to_string(),
                Style::default()
                    .fg(marker_fg)
                    .bg(base_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │  ", Style::default().fg(pal.border).bg(base_bg)),
        ];

        let gutter_w = 18usize;
        let code_w = (area.width as usize).saturating_sub(gutter_w);
        spans.extend(style_code_segments(
            &r.segs,
            r.row_bg.as_deref(),
            base_bg,
            &pal,
            h_scroll,
            code_w,
        ));

        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(base_bg)),
            row_rect,
        );
        app.hitboxes.push(HitBox {
            rect: row_rect,
            action: HitAction::SelectDiffRow {
                row_idx,
                side: "both".to_string(),
            },
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn render_split_half(
    cell: Option<&SplitCell>,
    width: usize,
    h_scroll: usize,
    is_selected: bool,
    is_queued: bool,
    in_chunk: bool,
    in_active_chunk: bool,
    pal: &ThemePalette,
) -> Vec<Span<'static>> {
    let rail = |bg: Color| -> Span<'static> {
        if is_selected {
            Span::styled("▌ ", Style::default().fg(pal.ask_text).bg(bg))
        } else if is_queued {
            Span::styled("▌ ", Style::default().fg(pal.move_text).bg(bg))
        } else if in_active_chunk {
            Span::styled("▌ ", Style::default().fg(pal.chunk_active_rail).bg(bg))
        } else if in_chunk {
            Span::styled("▏ ", Style::default().fg(pal.chunk_rail).bg(bg))
        } else {
            Span::styled("  ", Style::default().bg(bg))
        }
    };

    let Some(c) = cell else {
        return vec![
            rail(pal.empty_split),
            Span::styled(
                " ".repeat(width.saturating_sub(2)),
                Style::default().bg(pal.empty_split),
            ),
        ];
    };

    let base_bg = if is_selected {
        pal.ask_row_bg
    } else if is_queued {
        pal.queued_row_bg
    } else {
        match c.row_bg.as_deref() {
            Some("i") => pal.row_i,
            Some("d") => pal.row_d,
            Some("u") => pal.row_u,
            Some("m") => pal.row_m,
            _ => pal.bg,
        }
    };

    let marker_fg = match c.marker.as_str() {
        "+" => pal.add_text,
        "-" => pal.del_text,
        _ => pal.line_num,
    };
    let num_fg = if in_chunk || c.row_bg.is_some() {
        pal.line_num_active
    } else {
        pal.line_num
    };

    let mut spans = vec![
        rail(base_bg),
        Span::styled(
            format!("{:>4}  ", c.line_no),
            Style::default().fg(num_fg).bg(base_bg),
        ),
        Span::styled(
            c.marker.clone(),
            Style::default()
                .fg(marker_fg)
                .bg(base_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │  ", Style::default().fg(pal.border).bg(base_bg)),
    ];

    let gutter_w = 13usize;
    let code_w = width.saturating_sub(gutter_w);
    spans.extend(style_code_segments(
        &c.segs,
        c.row_bg.as_deref(),
        base_bg,
        pal,
        h_scroll,
        code_w,
    ));
    spans
}

fn draw_split_code_diff(
    f: &mut Frame<'_>,
    app: &mut AppState,
    diff: &DiffResponse,
    area: Rect,
) {
    let pal = app.palette.clone();
    let rows = &diff.split_rows;
    if rows.is_empty() {
        return;
    }

    let max_scroll = rows.len().saturating_sub(area.height as usize);
    if app.v_scroll > max_scroll {
        app.v_scroll = max_scroll;
    }

    let h_scroll = if app.wrap_lines { 0 } else { app.h_scroll };
    let total_w = area.width as usize;
    let left_w = total_w.saturating_sub(1) / 2;
    let right_w = total_w.saturating_sub(1).saturating_sub(left_w);

    for (vis_i, (row_idx, r)) in rows
        .iter()
        .enumerate()
        .skip(app.v_scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let ry = area.y + vis_i as u16;
        let row_rect = Rect {
            x: area.x,
            y: ry,
            width: area.width,
            height: 1,
        };

        if r.kind == "spacer" || r.kind == "chunk" {
            let line = Line::from(vec![
                Span::styled(" ".repeat(left_w), Style::default().bg(pal.bg)),
                Span::styled("│", Style::default().fg(pal.border).bg(pal.bg)),
            ]);
            f.render_widget(Paragraph::new(line), row_rect);
            continue;
        }

        if r.kind == "elide" {
            render_elide_row(
                f,
                app,
                r.fold_id.as_deref(),
                r.count.unwrap_or(0),
                row_rect,
            );
            continue;
        }

        let in_chunk = r.chunk_idx.is_some();
        let in_active_chunk = r.chunk_idx == Some(app.active_chunk);
        let is_selected = app
            .selection
            .as_ref()
            .is_some_and(|s| s.split_keys.contains(&r.key));
        let is_queued = app
            .ai_queue
            .iter()
            .any(|q| q.path == diff.path && q.split_keys.contains(&r.key));

        let mut spans = render_split_half(
            r.left.as_ref(),
            left_w,
            h_scroll,
            is_selected,
            is_queued,
            in_chunk,
            in_active_chunk,
            &pal,
        );
        spans.push(Span::styled("│", Style::default().fg(pal.border).bg(pal.bg)));
        spans.extend(render_split_half(
            r.right.as_ref(),
            right_w,
            h_scroll,
            is_selected,
            is_queued,
            in_chunk,
            in_active_chunk,
            &pal,
        ));

        f.render_widget(Paragraph::new(Line::from(spans)), row_rect);

        app.hitboxes.push(HitBox {
            rect: Rect {
                x: area.x,
                y: ry,
                width: left_w as u16,
                height: 1,
            },
            action: HitAction::SelectDiffRow {
                row_idx,
                side: "before".to_string(),
            },
        });
        app.hitboxes.push(HitBox {
            rect: Rect {
                x: area.x + left_w as u16 + 1,
                y: ry,
                width: right_w as u16,
                height: 1,
            },
            action: HitAction::SelectDiffRow {
                row_idx,
                side: "after".to_string(),
            },
        });
    }
}

fn draw_bottom_bar(f: &mut Frame<'_>, app: &mut AppState, area: Rect) {
    let pal = &app.palette;
    if app.composer == ComposerMode::AskAi {
        f.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(pal.ask_text))
                .style(Style::default().bg(pal.surface)),
            area,
        );
        let r0 = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };
        let mut s0 = Vec::new();
        let mut x0 = r0.x;

        let target_lbl = app
            .selection
            .as_ref()
            .map(|s| s.short_ref.clone())
            .unwrap_or_else(|| "Current Hunk".to_string());
        let tl = format!("  💬 Ask AI · {target_lbl}  ");
        let tw = tl.chars().count() as u16;
        s0.push(Span::styled(
            tl,
            Style::default()
                .fg(pal.ask_text)
                .bg(pal.surface)
                .add_modifier(Modifier::BOLD),
        ));
        x0 += tw;

        let agent_lbl = app
            .agents
            .get(app.selected_agent_idx)
            .map(|a| format!(" @ {} ({}) ", a.label, a.status))
            .unwrap_or_else(|| " @ Select Agent ".to_string());
        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            agent_lbl,
            Style::default().fg(pal.fg).bg(pal.raised),
            HitAction::CycleAgent,
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));

        if !app.ai_queue.is_empty() {
            push_button(
                &mut s0,
                &mut app.hitboxes,
                r0.y,
                &mut x0,
                format!(" Queued ({}) × ", app.ai_queue.len()),
                Style::default().fg(pal.move_text).bg(pal.raised),
                HitAction::ClearAiQueue,
            );
            push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));
        }

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " + Queue (Ctrl+Q) ".to_string(),
            Style::default().fg(pal.add_text).bg(pal.raised),
            HitAction::QueueAiPrompt,
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " ⏎ Send (Enter) ".to_string(),
            Style::default()
                .fg(Color::Rgb(12, 16, 26))
                .bg(pal.ask_text)
                .add_modifier(Modifier::BOLD),
            HitAction::SendAiPrompt,
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " Esc ".to_string(),
            Style::default().fg(pal.muted).bg(pal.raised),
            HitAction::CloseComposer,
        );
        f.render_widget(Paragraph::new(Line::from(s0)), r0);

        let r1 = Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: 1,
        };
        let prompt_line = Line::from(vec![
            Span::styled("  > ", Style::default().fg(pal.ask_text).bg(pal.bg)),
            Span::styled(
                format!("{}█", app.input_buffer),
                Style::default().fg(pal.fg).bg(pal.bg),
            ),
        ]);
        f.render_widget(
            Paragraph::new(prompt_line).style(Style::default().bg(pal.bg)),
            r1,
        );
        return;
    }

    if app.composer == ComposerMode::Commit {
        f.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(pal.add_text))
                .style(Style::default().bg(pal.surface)),
            area,
        );
        let r0 = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };
        let mut s0 = Vec::new();
        let mut x0 = r0.x;

        let title = "  ✓ Commit Changes  ";
        let tw = title.chars().count() as u16;
        s0.push(Span::styled(
            title,
            Style::default()
                .fg(pal.add_text)
                .bg(pal.surface)
                .add_modifier(Modifier::BOLD),
        ));
        x0 += tw;

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " Commit Staged (Enter) ".to_string(),
            Style::default()
                .fg(Color::Rgb(12, 16, 26))
                .bg(pal.add_text)
                .add_modifier(Modifier::BOLD),
            HitAction::RunCommit { stage_all: false },
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " Stage All & Commit (Ctrl+A) ".to_string(),
            Style::default().fg(pal.ask_text).bg(pal.raised),
            HitAction::RunCommit { stage_all: true },
        );
        push_sep(&mut s0, &mut x0, "  ", Style::default().bg(pal.surface));

        push_button(
            &mut s0,
            &mut app.hitboxes,
            r0.y,
            &mut x0,
            " Esc ".to_string(),
            Style::default().fg(pal.muted).bg(pal.raised),
            HitAction::CloseComposer,
        );
        f.render_widget(Paragraph::new(Line::from(s0)), r0);

        let r1 = Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: 1,
        };
        let msg_line = Line::from(vec![
            Span::styled(
                "  Commit message: ",
                Style::default().fg(pal.add_text).bg(pal.bg),
            ),
            Span::styled(
                format!("{}█", app.input_buffer),
                Style::default().fg(pal.fg).bg(pal.bg),
            ),
        ]);
        f.render_widget(
            Paragraph::new(msg_line).style(Style::default().bg(pal.bg)),
            r1,
        );
        return;
    }

    f.render_widget(
        Block::default().style(Style::default().bg(pal.chrome)),
        area,
    );
    if let Some((msg, is_err, ts)) = &app.toast
        && ts.elapsed() < Duration::from_secs(4)
    {
        let fg = if *is_err { pal.del_text } else { pal.add_text };
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("   ◆ {msg}"),
                Style::default()
                    .fg(fg)
                    .bg(pal.chrome)
                    .add_modifier(Modifier::BOLD),
            )),
            area,
        );
        return;
    }

    let mut spans = vec![Span::styled("   ", Style::default().bg(pal.chrome))];
    let mut fx = area.x + 3;

    for (k, desc) in [
        ("j/k", "file"),
        ("n/p", "hunk"),
        ("s", "stage"),
        ("x", "revert"),
        ("c", "commit"),
        ("a", "ask AI"),
        ("Tab", "view"),
        ("q", "close"),
    ] {
        let kw = k.chars().count() as u16;
        let dw = desc.chars().count() as u16;
        spans.push(Span::styled(
            k,
            Style::default()
                .fg(pal.fg)
                .bg(pal.chrome)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {desc}    "),
            Style::default().fg(pal.dim).bg(pal.chrome),
        ));
        fx += kw + 1 + dw + 4;
    }
    pad_spans_to_width(&mut spans, fx.saturating_sub(area.x), area.width, pal.chrome);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
