use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::rex_bridge::find_rex_bin;

const REQUIRE_LINE: &str = "require(\"omnidiff\")";

pub fn default_bin_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local").join("bin")
}

pub fn install(repo_dir: &Path, rex_home: &Path, skip_rex_cli: bool) -> Result<PathBuf> {
    let bin_dir = default_bin_dir();
    install_with_bin_dir(repo_dir, rex_home, &bin_dir, skip_rex_cli)
}

pub fn install_with_bin_dir(
    repo_dir: &Path,
    rex_home: &Path,
    bin_dir: &Path,
    skip_rex_cli: bool,
) -> Result<PathBuf> {
    let lua_src = repo_dir.join("lua").join("omnidiff.lua");
    if !lua_src.is_file() {
        bail!(
            "Could not find lua/omnidiff.lua in {}",
            repo_dir.display()
        );
    }

    fs::create_dir_all(bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let target_bin = bin_dir.join("rex-omnidiff");

    if let Ok(exe) = env::current_exe()
        && exe.is_file()
        && exe != target_bin
    {
        let tmp_bin = bin_dir.join(".rex-omnidiff.tmp");
        if fs::copy(&exe, &tmp_bin).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755));
            }
            let _ = fs::rename(&tmp_bin, &target_bin);
        }
    }

    let effective_bin = if target_bin.is_file() {
        target_bin.clone()
    } else {
        env::current_exe().unwrap_or_else(|_| target_bin.clone())
    };
    let bin_str = effective_bin.to_string_lossy().into_owned();

    fs::create_dir_all(rex_home)
        .with_context(|| format!("failed to create {}", rex_home.display()))?;
    let data_dir = rex_home.join("data").join("omnidiff");
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create {}", data_dir.display()))?;

    let lua_template = fs::read_to_string(&lua_src)
        .with_context(|| format!("failed to read {}", lua_src.display()))?;
    let rendered_lua = lua_template.replace("@REX_OMNIDIFF_BIN@", &bin_str);

    let dest_lua = rex_home.join("omnidiff.lua");
    let tmp_lua = rex_home.join(".omnidiff.lua.tmp");
    fs::write(&tmp_lua, rendered_lua)
        .with_context(|| format!("failed to write {}", tmp_lua.display()))?;
    fs::rename(&tmp_lua, &dest_lua)
        .with_context(|| format!("failed to install {}", dest_lua.display()))?;

    let init_lua = rex_home.join("init.lua");
    let existing_init = fs::read_to_string(&init_lua).unwrap_or_default();
    let has_require = existing_init.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.starts_with("--")
            && (trimmed.contains("require(\"omnidiff\")")
                || trimmed.contains("require('omnidiff')")
                || trimmed.contains("require \"omnidiff\"")
                || trimmed.contains("require 'omnidiff'"))
    });

    if !has_require {
        let mut new_init = existing_init;
        if !new_init.is_empty() && !new_init.ends_with('\n') {
            new_init.push('\n');
        }
        new_init.push_str(REQUIRE_LINE);
        new_init.push('\n');
        let tmp_init = rex_home.join(".init.lua.tmp");
        fs::write(&tmp_init, new_init)?;
        fs::rename(&tmp_init, &init_lua)?;
    }

    if !skip_rex_cli {
        let rex_bin = find_rex_bin();
        let check = Command::new(&rex_bin).args(["config", "check"]).output();
        if let Ok(out) = check
            && !out.status.success()
        {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("rex config check failed: {}", stderr.trim());
        }
        let _ = Command::new(&rex_bin).args(["config", "reload"]).output();
    }

    Ok(effective_bin)
}

pub fn uninstall(rex_home: &Path, skip_rex_cli: bool) -> Result<()> {
    let dest_lua = rex_home.join("omnidiff.lua");
    if dest_lua.exists() {
        let _ = fs::remove_file(&dest_lua);
    }

    let data_dir = rex_home.join("data").join("omnidiff");
    if data_dir.exists() {
        let _ = fs::remove_dir_all(&data_dir);
    }

    let init_lua = rex_home.join("init.lua");
    if init_lua.is_file()
        && let Ok(existing) = fs::read_to_string(&init_lua)
    {
        let filtered: Vec<&str> = existing
            .lines()
            .filter(|line| {
                let t = line.trim();
                !(t == "require(\"omnidiff\")"
                    || t == "require('omnidiff')"
                    || t == "require \"omnidiff\""
                    || t == "require 'omnidiff'")
            })
            .collect();
        let mut updated = filtered.join("\n");
        if !updated.is_empty() {
            updated.push('\n');
        }
        let tmp_init = rex_home.join(".init.lua.tmp");
        fs::write(&tmp_init, updated)?;
        fs::rename(&tmp_init, &init_lua)?;
    }

    if !skip_rex_cli {
        let rex_bin = find_rex_bin();
        let _ = Command::new(&rex_bin).args(["config", "reload"]).output();
    }

    Ok(())
}
