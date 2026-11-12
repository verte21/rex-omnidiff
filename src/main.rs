mod actions;
mod diff_engine;
mod git;
mod installer;
mod kitty;
mod rex_bridge;
mod theme;
mod types;
mod ui;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::actions::{
    cleanup_scratch, commit_changes, discard_file, load_undo_summary, pop_undo,
    prepare_tui_launch, revert_chunk, stage_chunk, stage_file, toggle_review, unstage_chunk,
    unstage_file,
};
use crate::diff_engine::{DiffParams, compute_diff};
use crate::git::{git_write, has_head, load_commit_files, load_status, repo_root, resolve_rex_home};
use crate::installer::{install, uninstall};
use crate::ui::{UiOptions, run_ui};

fn ensure_standard_path() {
    let current = env::var("PATH").unwrap_or_default();
    let home = env::var("HOME").unwrap_or_default();
    let mut extras = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];
    if !home.is_empty() {
        extras.insert(0, format!("{home}/.local/bin"));
        extras.insert(1, format!("{home}/.cargo/bin"));
    }
    let existing: Vec<String> = current.split(':').map(str::to_string).collect();
    let mut merged = existing.clone();
    for e in extras {
        if !merged.iter().any(|x| x == &e) {
            merged.push(e);
        }
    }
    let joined = merged.join(":");
    if joined != current {
        // Safe at startup before spawning any threads
        unsafe {
            env::set_var("PATH", joined);
        }
    }
}

fn flag_val(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn flag_usize(args: &[String], name: &str, default: usize) -> usize {
    flag_val(args, name)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run() -> Result<()> {
    ensure_standard_path();
    let raw_args: Vec<String> = env::args().skip(1).collect();

    if has_flag(&raw_args, "--help") || has_flag(&raw_args, "-h") {
        println!(
            "rex-omnidiff — Structural AST & Kitty Graphics diff workspace for Rex\n\n\
             Usage:\n  \
               rex-omnidiff ui [--mode float|dock] [--source-block <id>] [--cwd <dir>]\n  \
               rex-omnidiff install [--repo-dir <dir>] [--rex-home <dir>]\n  \
               rex-omnidiff uninstall [--rex-home <dir>]\n  \
               rex-omnidiff api <status|diff|stage-file|stage-chunk|...> [options]"
        );
        return Ok(());
    }

    let first = raw_args.first().map(String::as_str).unwrap_or("ui");

    match first {
        "ui" => {
            let rest = if raw_args.is_empty() {
                &[][..]
            } else {
                &raw_args[1..]
            };
            let cwd = flag_val(rest, "--cwd")
                .map(PathBuf::from)
                .or_else(|| env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let rex_home = resolve_rex_home(
                flag_val(rest, "--rex-home")
                    .or_else(|| flag_val(rest, "--herdr-home"))
                    .as_deref(),
            );
            let mode = flag_val(rest, "--mode").unwrap_or_else(|| "float".to_string());
            let source_block = flag_val(rest, "--source-block");
            let initial_scope = flag_val(rest, "--scope");
            let initial_path = flag_val(rest, "--path");

            return run_ui(UiOptions {
                cwd,
                rex_home,
                mode,
                source_block,
                initial_scope,
                initial_path,
            });
        }
        "install" => {
            let rest = &raw_args[1..];
            let repo_dir = flag_val(rest, "--repo-dir")
                .map(PathBuf::from)
                .or_else(|| env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let rex_home = resolve_rex_home(flag_val(rest, "--rex-home").as_deref());
            let skip_rex = has_flag(rest, "--skip-rex-cli");
            let installed_bin = install(&repo_dir, &rex_home, skip_rex)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "ok": true,
                    "binary": installed_bin,
                    "rex_home": rex_home,
                }))?
            );
            return Ok(());
        }
        "uninstall" => {
            let rest = &raw_args[1..];
            let rex_home = resolve_rex_home(flag_val(rest, "--rex-home").as_deref());
            let skip_rex = has_flag(rest, "--skip-rex-cli");
            uninstall(&rex_home, skip_rex)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "ok": true,
                    "rex_home": rex_home,
                }))?
            );
            return Ok(());
        }
        _ => {}
    }

    let api_args = if first == "api" {
        &raw_args[1..]
    } else {
        &raw_args[..]
    };

    let sub = api_args
        .first()
        .map(String::as_str)
        .context("missing subcommand")?;
    let rest = &api_args[1..];

    let cwd = flag_val(rest, "--cwd")
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let app_home = resolve_rex_home(
        flag_val(rest, "--rex-home")
            .or_else(|| flag_val(rest, "--herdr-home"))
            .as_deref(),
    );

    match sub {
        "status" => {
            let commit_limit = flag_usize(rest, "--commits", 40);
            let resp = load_status(&cwd, &app_home, commit_limit)?;
            println!("{}", serde_json::to_string(&resp)?);
        }
        "commit-files" => {
            let hash = flag_val(rest, "--commit").context("missing --commit <sha>")?;
            let resp = load_commit_files(&cwd, &hash)?;
            println!("{}", serde_json::to_string(&resp)?);
        }
        "diff" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path").context("missing --path <file>")?;
            let old_path = flag_val(rest, "--old-path");
            let commit_hash = flag_val(rest, "--commit");
            let preset = flag_val(rest, "--preset").unwrap_or_else(|| "default".to_string());
            let context_lines = flag_usize(rest, "--context", 3);
            let expanded_folds: HashSet<String> = flag_val(rest, "--expand")
                .map(|s| {
                    s.split(',')
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            let params = DiffParams {
                cwd: &cwd,
                app_home: &app_home,
                scope: &scope,
                path: &path,
                old_path: old_path.as_deref(),
                commit_hash: commit_hash.as_deref(),
                preset: &preset,
                context_lines,
                expanded_folds: &expanded_folds,
            };
            let resp = compute_diff(&params)?;
            println!("{}", serde_json::to_string(&resp)?);
        }
        "stage-file" => {
            let path = flag_val(rest, "--path").context("missing --path")?;
            let old_path = flag_val(rest, "--old-path");
            stage_file(&cwd, &path, old_path.as_deref())?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "stage-all" => {
            let root = repo_root(&cwd)?;
            git_write(&root, &["add", "-A"])?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "unstage-file" => {
            let path = flag_val(rest, "--path").context("missing --path")?;
            let old_path = flag_val(rest, "--old-path");
            unstage_file(&cwd, &path, old_path.as_deref())?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "unstage-all" => {
            let root = repo_root(&cwd)?;
            if has_head(&root) {
                git_write(&root, &["restore", "--staged", "."])?;
            } else {
                git_write(&root, &["rm", "--cached", "-r", "-f", "."])?;
            }
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "discard-file" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path").context("missing --path")?;
            discard_file(&cwd, &app_home, &scope, &path)?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "stage-chunk" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path").context("missing --path")?;
            let old_path = flag_val(rest, "--old-path");
            let b_start = flag_usize(rest, "--b-start", 0);
            let b_end = flag_usize(rest, "--b-end", 0);
            let a_start = flag_usize(rest, "--a-start", 0);
            let a_end = flag_usize(rest, "--a-end", 0);
            let sig = flag_val(rest, "--sig");
            stage_chunk(
                &cwd,
                &scope,
                &path,
                old_path.as_deref(),
                b_start,
                b_end,
                a_start,
                a_end,
                sig.as_deref(),
            )?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "unstage-chunk" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path").context("missing --path")?;
            let old_path = flag_val(rest, "--old-path");
            let b_start = flag_usize(rest, "--b-start", 0);
            let b_end = flag_usize(rest, "--b-end", 0);
            let a_start = flag_usize(rest, "--a-start", 0);
            let a_end = flag_usize(rest, "--a-end", 0);
            let sig = flag_val(rest, "--sig");
            unstage_chunk(
                &cwd,
                &scope,
                &path,
                old_path.as_deref(),
                b_start,
                b_end,
                a_start,
                a_end,
                sig.as_deref(),
            )?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "revert-chunk" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path").context("missing --path")?;
            let old_path = flag_val(rest, "--old-path");
            let b_start = flag_usize(rest, "--b-start", 0);
            let b_end = flag_usize(rest, "--b-end", 0);
            let a_start = flag_usize(rest, "--a-start", 0);
            let a_end = flag_usize(rest, "--a-end", 0);
            let expected_hash = flag_val(rest, "--expected-hash");
            let sig = flag_val(rest, "--sig");
            revert_chunk(
                &cwd,
                &app_home,
                &scope,
                &path,
                old_path.as_deref(),
                b_start,
                b_end,
                a_start,
                a_end,
                expected_hash.as_deref(),
                sig.as_deref(),
            )?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "review-toggle" => {
            let path = flag_val(rest, "--path").context("missing --path")?;
            let chunk_sig = flag_val(rest, "--chunk-sig");
            let file_sig = flag_val(rest, "--file-sig");
            let reviewed = toggle_review(
                &cwd,
                &app_home,
                &path,
                chunk_sig.as_deref(),
                file_sig.as_deref(),
            )?;
            println!(
                "{}",
                serde_json::to_string(&json!({ "ok": true, "reviewed": reviewed }))?
            );
        }
        "undo" => {
            let undone = pop_undo(&cwd, &app_home)?;
            let root = repo_root(&cwd)?;
            let (undo_top, undo_count) = load_undo_summary(&app_home, &root.to_string_lossy());
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "ok": true,
                    "undone": undone,
                    "undo_top": undo_top,
                    "undo_count": undo_count,
                }))?
            );
        }
        "commit" => {
            let msg = flag_val(rest, "--message").context("missing --message")?;
            let stage_all = has_flag(rest, "--stage-all");
            commit_changes(&cwd, &msg, stage_all)?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        "prepare-tui" => {
            let scope = flag_val(rest, "--scope").unwrap_or_else(|| "all".to_string());
            let path = flag_val(rest, "--path");
            let old_path = flag_val(rest, "--old-path");
            let commit_hash = flag_val(rest, "--commit");
            let preset = flag_val(rest, "--preset").unwrap_or_else(|| "default".to_string());
            let palette_json = flag_val(rest, "--palette");
            let spec = prepare_tui_launch(
                &cwd,
                &app_home,
                &scope,
                path.as_deref(),
                old_path.as_deref(),
                commit_hash.as_deref(),
                &preset,
                palette_json.as_deref(),
            )?;
            println!("{}", serde_json::to_string(&spec)?);
        }
        "cleanup" => {
            cleanup_scratch(&app_home)?;
            println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
        }
        other => {
            bail!("unknown subcommand '{other}'");
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let err_json = json!({
                "ok": false,
                "error": format!("{e:#}"),
            });
            eprintln!("{err_json}");
            ExitCode::from(1)
        }
    }
}
