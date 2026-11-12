use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::actions::{load_reviewed_map, load_undo_summary, make_file_review_key};
use crate::types::{
    CommitEntry, CommitFilesResponse, FileEntry, PictureMedia, StatusResponse,
};

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

const READ_FLAGS: &[&str] = &[
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=false",
];

pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn short_sha256(bytes: &[u8]) -> String {
    let full = hex_sha256(bytes);
    full[..16].to_string()
}

pub fn resolve_rex_home(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(p) = env::var("REX_CONFIG_DIR")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("rex");
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("rex")
}

pub fn find_omnidiff_bin() -> Option<String> {
    if let Ok(bin) = env::var("OMNIDIFF_BIN")
        && !bin.is_empty()
        && Path::new(&bin).is_file()
    {
        return Some(bin);
    }
    let home = env::var("HOME").unwrap_or_default();
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/omnidiff"),
        PathBuf::from("/usr/local/bin/omnidiff"),
    ];
    if !home.is_empty() {
        candidates.push(PathBuf::from(&home).join(".cargo/bin/omnidiff"));
        candidates.push(PathBuf::from(&home).join(".local/bin/omnidiff"));
        candidates.push(PathBuf::from(&home).join(".nix-profile/bin/omnidiff"));
    }
    candidates.push(PathBuf::from("/run/current-system/sw/bin/omnidiff"));
    candidates.push(PathBuf::from("/usr/bin/omnidiff"));

    for c in candidates {
        if c.is_file() {
            return Some(c.to_string_lossy().into_owned());
        }
    }
    if let Ok(path_var) = env::var("PATH") {
        for dir in env::split_paths(&path_var) {
            let p = dir.join("omnidiff");
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

pub fn git_read(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(READ_FLAGS)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim().lines().next().unwrap_or("unknown error")
        );
    }
    Ok(out.stdout)
}

pub fn git_write(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let msg = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        bail!("git {} failed: {}", args.join(" "), msg);
    }
    Ok(out.stdout)
}

pub fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let out = git_read(cwd, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not a git repository", cwd.display()))?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
}

pub fn has_head(root: &Path) -> bool {
    git_read(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawStatusItem {
    pub(crate) status: String,
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NumStat {
    pub(crate) add: usize,
    pub(crate) del: usize,
    pub(crate) binary: bool,
}

/// Parses combined `git diff --raw --numstat -M -z` output in a single pass.
pub(crate) fn parse_raw_and_numstat_z(
    bytes: &[u8],
) -> (Vec<RawStatusItem>, HashMap<String, NumStat>) {
    let mut items: Vec<RawStatusItem> = Vec::new();
    let mut item_idx: HashMap<String, usize> = HashMap::new();
    let mut stats: HashMap<String, NumStat> = HashMap::new();
    let mut parts = bytes.split(|&b| b == 0).filter(|s| !s.is_empty());

    while let Some(entry) = parts.next() {
        let text = String::from_utf8_lossy(entry);
        if text.starts_with(':') && !text.contains('\t') {
            let status_tok = text.split_whitespace().last().unwrap_or("M");
            let code = status_tok.chars().next().unwrap_or('M');
            let status = code.to_string();
            let Some(first) = parts.next() else { break };
            let first_str = String::from_utf8_lossy(first).into_owned();
            let (path, old_path) = if matches!(code, 'R' | 'C') {
                let Some(second) = parts.next() else { break };
                (String::from_utf8_lossy(second).into_owned(), Some(first_str))
            } else {
                (first_str, None)
            };
            if let Some(&idx) = item_idx.get(&path) {
                if status == "U" {
                    items[idx].status = status;
                }
            } else {
                item_idx.insert(path.clone(), items.len());
                items.push(RawStatusItem {
                    status,
                    path,
                    old_path,
                });
            }
            continue;
        }

        let mut cols = text.splitn(3, '\t');
        let (Some(a_str), Some(d_str), Some(rest)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        let binary = a_str == "-" || d_str == "-";
        let add = a_str.parse::<usize>().unwrap_or(0);
        let del = d_str.parse::<usize>().unwrap_or(0);
        let path = if rest.is_empty() {
            let _old = parts.next();
            match parts.next() {
                Some(new_p) => String::from_utf8_lossy(new_p).into_owned(),
                None => continue,
            }
        } else {
            rest.to_string()
        };
        let prev = stats.entry(path).or_default();
        prev.add += add;
        prev.del += del;
        prev.binary |= binary;
    }

    (items, stats)
}

fn parse_untracked_z(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

const LOG_FORMAT: &str = "--format=%H%x1f%h%x1f%an%x1f%ad%x1f%s";

fn parse_commits_z(bytes: &[u8]) -> Vec<CommitEntry> {
    bytes
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .filter_map(|rec| {
            let text = String::from_utf8_lossy(rec);
            let mut f = text.trim_end_matches('\n').split('\x1f');
            Some(CommitEntry {
                hash: f.next()?.to_string(),
                short: f.next()?.to_string(),
                author: f.next()?.to_string(),
                date: f.next()?.to_string(),
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn untracked_stat(root: &Path, rel_path: &str) -> NumStat {
    let full = root.join(rel_path);
    let Ok(meta) = fs::metadata(&full) else {
        return NumStat::default();
    };
    if !meta.is_file() || meta.len() > 512 * 1024 {
        return NumStat::default();
    }
    let Ok(bytes) = fs::read(&full) else {
        return NumStat::default();
    };
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return NumStat {
            add: 0,
            del: 0,
            binary: true,
        };
    }
    let mut lines = bytes.iter().filter(|&&b| b == b'\n').count();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        lines += 1;
    }
    NumStat {
        add: lines,
        del: 0,
        binary: false,
    }
}

pub fn load_status(cwd: &Path, app_home: &Path, commit_limit: usize) -> Result<StatusResponse> {
    let root = repo_root(cwd)?;
    let head_exists = has_head(&root);

    let (branch_res, unstaged_res, untracked_res, staged_res, all_res, commits_res) =
        std::thread::scope(|s| {
            let root_ref = &root;
            let t_branch = s.spawn(move || {
                git_read(root_ref, &["branch", "--show-current"])
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).trim().to_string())
                    .filter(|s| !s.is_empty())
            });
            let t_unstaged =
                s.spawn(move || git_read(root_ref, &["diff", "--raw", "--numstat", "-M", "-z"]));
            let t_untracked = s.spawn(move || {
                git_read(
                    root_ref,
                    &["ls-files", "--others", "--exclude-standard", "-z"],
                )
            });
            let t_staged = s.spawn(move || {
                git_read(
                    root_ref,
                    &["diff", "--cached", "--raw", "--numstat", "-M", "-z"],
                )
            });
            let t_all = s.spawn(move || {
                if head_exists {
                    git_read(
                        root_ref,
                        &["diff", "HEAD", "--raw", "--numstat", "-M", "-z"],
                    )
                } else {
                    Ok(Vec::new())
                }
            });
            let t_commits = s.spawn(move || {
                if head_exists && commit_limit > 0 {
                    let lim = commit_limit.to_string();
                    git_read(
                        root_ref,
                        &["log", "-z", "-n", &lim, "--date=short", LOG_FORMAT],
                    )
                    .ok()
                } else {
                    None
                }
            });

            (
                t_branch.join().unwrap(),
                t_unstaged.join().unwrap(),
                t_untracked.join().unwrap(),
                t_staged.join().unwrap(),
                t_all.join().unwrap(),
                t_commits.join().unwrap(),
            )
        });

    let (unstaged_raw, unstaged_stats) = parse_raw_and_numstat_z(&unstaged_res?);
    let untracked_paths = parse_untracked_z(&untracked_res?);
    let (staged_raw, staged_stats) = parse_raw_and_numstat_z(&staged_res?);

    let staged_set: HashSet<String> = staged_raw.iter().map(|i| i.path.clone()).collect();
    let mut unstaged_set: HashSet<String> = unstaged_raw.iter().map(|i| i.path.clone()).collect();
    for p in &untracked_paths {
        unstaged_set.insert(p.clone());
    }

    let reviewed_map = load_reviewed_map(app_home);
    let root_str = root.to_string_lossy().into_owned();
    let mut reviewed_cache: HashMap<String, bool> = HashMap::new();
    let mut is_path_reviewed = |path: &str| -> bool {
        if let Some(&cached) = reviewed_cache.get(path) {
            return cached;
        }
        let key = make_file_review_key(&root_str, path);
        let res = match reviewed_map.files.get(&key) {
            None => false,
            Some(saved_hash) => {
                let full = root.join(path);
                match fs::read(&full) {
                    Ok(bytes) => &short_sha256(&bytes) == saved_hash,
                    Err(_) => saved_hash == "DELETED",
                }
            }
        };
        reviewed_cache.insert(path.to_string(), res);
        res
    };

    let untracked_limited: Vec<(&String, NumStat)> = untracked_paths
        .iter()
        .take(1000)
        .enumerate()
        .map(|(idx, path)| {
            let st = if idx < 150 {
                untracked_stat(&root, path)
            } else {
                NumStat::default()
            };
            (path, st)
        })
        .collect();

    let mut unstaged = Vec::new();
    for item in unstaged_raw.into_iter().take(1000) {
        let st = unstaged_stats.get(&item.path).copied().unwrap_or_default();
        let reviewed = is_path_reviewed(&item.path);
        let sp = staged_set.contains(&item.path);
        unstaged.push(FileEntry {
            path: item.path,
            old_path: item.old_path,
            status: item.status,
            untracked: false,
            add: st.add,
            del: st.del,
            binary: st.binary,
            staged_part: sp,
            unstaged_part: true,
            reviewed,
        });
    }
    for &(path, st) in &untracked_limited {
        unstaged.push(FileEntry {
            path: path.clone(),
            old_path: None,
            status: "?".to_string(),
            untracked: true,
            add: st.add,
            del: st.del,
            binary: st.binary,
            staged_part: false,
            unstaged_part: true,
            reviewed: is_path_reviewed(path),
        });
    }

    let mut staged = Vec::new();
    for item in staged_raw.into_iter().take(1000) {
        let st = staged_stats.get(&item.path).copied().unwrap_or_default();
        let reviewed = is_path_reviewed(&item.path);
        let up = unstaged_set.contains(&item.path);
        staged.push(FileEntry {
            path: item.path,
            old_path: item.old_path,
            status: item.status,
            untracked: false,
            add: st.add,
            del: st.del,
            binary: st.binary,
            staged_part: true,
            unstaged_part: up,
            reviewed,
        });
    }

    let mut all = Vec::new();
    if head_exists {
        let (all_raw, all_stats) = parse_raw_and_numstat_z(&all_res?);
        for item in all_raw.into_iter().take(1000) {
            let st = all_stats.get(&item.path).copied().unwrap_or_default();
            let sp = staged_set.contains(&item.path);
            let up = unstaged_set.contains(&item.path);
            let reviewed = is_path_reviewed(&item.path);
            all.push(FileEntry {
                path: item.path,
                old_path: item.old_path,
                status: item.status,
                untracked: false,
                add: st.add,
                del: st.del,
                binary: st.binary,
                staged_part: sp,
                unstaged_part: up,
                reviewed,
            });
        }
        for &(path, st) in &untracked_limited {
            all.push(FileEntry {
                path: path.clone(),
                old_path: None,
                status: "?".to_string(),
                untracked: true,
                add: st.add,
                del: st.del,
                binary: st.binary,
                staged_part: false,
                unstaged_part: true,
                reviewed: is_path_reviewed(path),
            });
        }
    } else {
        let mut seen = HashSet::new();
        for f in &staged {
            seen.insert(f.path.clone());
            all.push(f.clone());
        }
        for f in &unstaged {
            if !seen.contains(&f.path) {
                all.push(f.clone());
            }
        }
    }

    let commits = commits_res
        .as_deref()
        .map(parse_commits_z)
        .unwrap_or_default();
    let (undo_top, undo_count) = load_undo_summary(app_home, &root_str);

    Ok(StatusResponse {
        root: root_str,
        branch: branch_res,
        has_head: head_exists,
        omnidiff_bin: find_omnidiff_bin(),
        all,
        unstaged,
        staged,
        commits,
        undo_top,
        undo_count,
    })
}

pub fn commit_base(root: &Path, hash: &str) -> Result<String> {
    let out = git_read(root, &["rev-list", "--parents", "-n", "1", hash])?;
    let text = String::from_utf8_lossy(&out);
    Ok(text
        .split_whitespace()
        .nth(1)
        .unwrap_or(EMPTY_TREE)
        .to_string())
}

pub fn load_commit_files(cwd: &Path, hash: &str) -> Result<CommitFilesResponse> {
    let root = repo_root(cwd)?;
    let base = commit_base(&root, hash)?;
    let raw_num = git_read(
        root.as_path(),
        &["diff", "--raw", "--numstat", "-M", "-z", &base, hash],
    )?;
    let (raw, stats) = parse_raw_and_numstat_z(&raw_num);
    let files = raw
        .into_iter()
        .map(|item| {
            let st = stats.get(&item.path).copied().unwrap_or_default();
            FileEntry {
                path: item.path,
                old_path: item.old_path,
                status: item.status,
                untracked: false,
                add: st.add,
                del: st.del,
                binary: st.binary,
                staged_part: false,
                unstaged_part: false,
                reviewed: false,
            }
        })
        .collect();
    Ok(CommitFilesResponse {
        hash: hash.to_string(),
        base,
        files,
    })
}

pub fn git_blob(root: &Path, rev: &str, path: &str) -> Option<Vec<u8>> {
    let spec = if rev.is_empty() {
        format!(":{path}")
    } else {
        format!("{rev}:{path}")
    };
    git_read(root, &["show", &spec]).ok()
}

pub struct MaterializedPair {
    pub before_path: PathBuf,
    pub after_path: PathBuf,
    pub before_bytes: Vec<u8>,
    pub after_bytes: Vec<u8>,
    pub index_bytes: Option<Vec<u8>>,
    pub wt_exists: bool,
    pub worktree_sha256: Option<String>,
    pub picture_media: Option<PictureMedia>,
}

fn safe_rel_path(rel: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in Path::new(rel).components() {
        if let std::path::Component::Normal(os) = comp {
            out.push(os);
        }
    }
    if out.as_os_str().is_empty() {
        out.push("file");
    }
    out
}

fn write_scratch_file(base_dir: &Path, slot: &str, rel_path: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dest = base_dir.join(slot).join(safe_rel_path(rel_path));
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, bytes)?;
    Ok(dest)
}

pub fn is_previewable_image(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    for ext in ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico"] {
        if lower.ends_with(&format!(".{ext}")) {
            return Some(ext);
        }
    }
    None
}

fn stage_picture_media(
    app_home: &Path,
    path: &str,
    before_bytes: &[u8],
    after_bytes: &[u8],
) -> Option<PictureMedia> {
    let ext = is_previewable_image(path)?;
    let media_dir = app_home.join("data").join("omnidiff").join("media");
    let _ = fs::create_dir_all(&media_dir);

    let write_one = |bytes: &[u8]| -> Option<(String, String)> {
        if bytes.is_empty() {
            return None;
        }
        let name = format!("{}.{ext}", short_sha256(bytes));
        let full = media_dir.join(&name);
        if !full.exists() {
            fs::write(&full, bytes).ok()?;
        }
        Some((
            format!("data/omnidiff/media/{name}"),
            full.to_string_lossy().into_owned(),
        ))
    };

    let before_pair = write_one(before_bytes);
    let after_pair = write_one(after_bytes);
    if before_pair.is_none() && after_pair.is_none() {
        None
    } else {
        Some(PictureMedia {
            before_rel: before_pair.as_ref().map(|(r, _)| r.clone()),
            after_rel: after_pair.as_ref().map(|(r, _)| r.clone()),
            before_abs: before_pair.map(|(_, a)| a),
            after_abs: after_pair.map(|(_, a)| a),
        })
    }
}

pub fn materialize_pair(
    root: &Path,
    app_home: &Path,
    scratch_ns: &str,
    scope: &str,
    path: &str,
    old_path: Option<&str>,
    commit_hash: Option<&str>,
) -> Result<MaterializedPair> {
    let scratch_dir = app_home.join("data").join("omnidiff").join(scratch_ns);
    let prev_path = old_path.unwrap_or(path);
    let wt_full = root.join(path);
    let wt_exists = fs::symlink_metadata(&wt_full).is_ok();
    let wt_bytes = if wt_full.is_file() {
        Some(fs::read(&wt_full)?)
    } else {
        None
    };
    let worktree_sha256 = wt_bytes.as_deref().map(hex_sha256);

    let (before_bytes, after_bytes, index_bytes, use_live_after) = match scope {
        "unstaged" => {
            let idx = git_blob(root, "", prev_path);
            let b = idx.clone().unwrap_or_default();
            let a = wt_bytes.clone().unwrap_or_default();
            (b, a, idx, wt_bytes.is_some())
        }
        "staged" => {
            let b = if has_head(root) {
                git_blob(root, "HEAD", prev_path).unwrap_or_default()
            } else {
                Vec::new()
            };
            let idx = git_blob(root, "", path);
            let a = idx.clone().unwrap_or_default();
            (b, a, idx, false)
        }
        "commit" => {
            let hash = commit_hash.context("commit scope requires --commit <sha>")?;
            let base = commit_base(root, hash)?;
            let b = if base == EMPTY_TREE {
                Vec::new()
            } else {
                git_blob(root, &base, prev_path).unwrap_or_default()
            };
            let a = git_blob(root, hash, path).unwrap_or_default();
            (b, a, None, false)
        }
        _ => {
            // "all": Working Tree vs HEAD
            let b = if has_head(root) {
                git_blob(root, "HEAD", prev_path).unwrap_or_default()
            } else {
                Vec::new()
            };
            let idx = git_blob(root, "", path);
            let a = wt_bytes.clone().unwrap_or_default();
            (b, a, idx, wt_bytes.is_some())
        }
    };

    let before_path = write_scratch_file(&scratch_dir, "before", prev_path, &before_bytes)?;
    let after_path = if use_live_after {
        wt_full
    } else {
        write_scratch_file(&scratch_dir, "after", path, &after_bytes)?
    };

    let picture_media = stage_picture_media(app_home, path, &before_bytes, &after_bytes);

    Ok(MaterializedPair {
        before_path,
        after_path,
        before_bytes,
        after_bytes,
        index_bytes,
        wt_exists,
        worktree_sha256,
        picture_media,
    })
}
