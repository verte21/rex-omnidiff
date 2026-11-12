use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use image::{Rgba, RgbaImage};
use serde_json::json;
use tempfile::TempDir;

use crate::actions::{
    discard_file, pop_undo, revert_chunk, stage_chunk, stage_file, toggle_review, unstage_chunk,
    unstage_file,
};
use crate::diff_engine::{
    DiffParams, build_text_diff_response, compute_diff, slice_line_segments,
};
use crate::git::{hex_sha256, load_status};
use crate::kitty::{
    KITTY_IMAGE_ID, KittyImageMode, compute_image_diff_stats, encode_kitty_png_escape,
    render_composite_diff_image,
};
use crate::rex_bridge::{classify_block_agent, parse_file_url_path};
use crate::theme::{
    LineSyntaxState, RawTheme, build_palette, load_theme_from_bundle_json, tokenize_syntax_segment,
};
use crate::types::{OmniDiffJson, OmniHunk, OmniRange, OmniSide, TextSegment};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git failed to start");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_sample_repo() -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    let app_home = TempDir::new().unwrap();
    let p = repo.path();
    git(p, &["init"]);
    git(p, &["config", "user.email", "test@example.com"]);
    git(p, &["config", "user.name", "Test User"]);

    let src_dir = p.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let initial = "\
pub fn alpha(x: i32) -> i32 {
    let a = x + 1;
    a
}

pub fn middle() {
    println!(\"keep 1\");
    println!(\"keep 2\");
    println!(\"keep 3\");
    println!(\"keep 4\");
    println!(\"keep 5\");
    println!(\"keep 6\");
    println!(\"keep 7\");
    println!(\"keep 8\");
}

pub fn omega(y: i32) -> i32 {
    let b = y * 2;
    b
}
";
    fs::write(src_dir.join("lib.rs"), initial).unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "Initial commit"]);
    (repo, app_home)
}

#[test]
fn multibyte_utf8_and_tab_span_slicing_matches_byte_offsets() {
    let line = "\tlet s = \"zażółć 🦀\";   ";
    let spans = vec![(10, 30, "u")];
    let (segs, width) = slice_line_segments(line, &spans);

    assert_eq!(
        segs,
        vec![
            TextSegment {
                t: "    let s = \"".to_string(),
                op: None,
            },
            TextSegment {
                t: "zażółć 🦀\";".to_string(),
                op: Some("u".to_string()),
            },
            TextSegment {
                t: "   ".to_string(),
                op: None,
            },
        ]
    );
    assert!(width > 20);
}

#[test]
fn move_cross_linking_breadcrumbs_and_fold_expansion_work() {
    let before = "fn outer() {\n    moved_call();\n    stay_1();\n    stay_2();\n    stay_3();\n    stay_4();\n    stay_5();\n    stay_6();\n    stay_7();\n    stay_8();\n    end_anchor();\n}\n";
    let after = "fn outer() {\n    stay_1();\n    stay_2();\n    stay_3();\n    stay_4();\n    stay_5();\n    stay_6();\n    stay_7();\n    stay_8();\n    moved_call();\n    end_anchor();\n}\n";

    let omni = OmniDiffJson {
        before: OmniSide {
            path: "a.rs".into(),
            language: Some("Rust".into()),
            encoding: None,
            hunks: vec![OmniHunk {
                operation: "move".into(),
                range: OmniRange {
                    start_row: 1,
                    start_column: 4,
                    end_row: 1,
                    end_column: 17,
                },
                move_target: Some(OmniRange {
                    start_row: 9,
                    start_column: 4,
                    end_row: 9,
                    end_column: 17,
                }),
                reference_line: Some(0),
            }],
        },
        after: OmniSide {
            path: "b.rs".into(),
            language: Some("Rust".into()),
            encoding: None,
            hunks: vec![OmniHunk {
                operation: "move".into(),
                range: OmniRange {
                    start_row: 9,
                    start_column: 4,
                    end_row: 9,
                    end_column: 17,
                },
                move_target: Some(OmniRange {
                    start_row: 1,
                    start_column: 4,
                    end_row: 1,
                    end_column: 17,
                }),
                reference_line: Some(0),
            }],
        },
        large_residual: false,
        summary: Some("refactor_moved_only".into()),
        binary: None,
        content: None,
        picture: None,
    };

    let empty_set = HashSet::new();
    let resp = build_text_diff_response(
        "/repo",
        "src/a.rs",
        None,
        "all",
        before,
        after,
        Some(omni.clone()),
        "omnidiff".into(),
        None,
        "sig1".into(),
        false,
        &empty_set,
        2,
        &empty_set,
    );

    assert_eq!(resp.chunks.len(), 2);
    assert_eq!(resp.chunks[0].breadcrumb.as_deref(), Some("fn outer() {"));
    assert_eq!(
        resp.chunks[0]
            .move_info
            .as_ref()
            .and_then(|m| m.target_chunk),
        Some(1)
    );
    assert_eq!(
        resp.chunks[1]
            .move_info
            .as_ref()
            .and_then(|m| m.target_chunk),
        Some(0)
    );

    let elide_row = resp
        .unified_rows
        .iter()
        .find(|r| r.kind == "elide")
        .expect("expected folded unchanged lines");
    let fold_id = elide_row.fold_id.clone().unwrap();

    let mut expanded = HashSet::new();
    expanded.insert(fold_id);
    let resp_expanded = build_text_diff_response(
        "/repo",
        "src/a.rs",
        None,
        "all",
        before,
        after,
        Some(omni),
        "omnidiff".into(),
        None,
        "sig1".into(),
        false,
        &empty_set,
        2,
        &expanded,
    );
    assert!(resp_expanded.unified_rows.iter().all(|r| r.kind != "elide"));
}

#[test]
fn end_to_end_omnidiff_chunk_stage_unstage_revert_undo_and_review() {
    let (repo, app_home) = init_sample_repo();
    let root = repo.path();
    let home = app_home.path();

    let updated = "\
pub fn alpha(x: i32) -> i32 {
    let a = x + 100;
    a
}

pub fn middle() {
    println!(\"keep 1\");
    println!(\"keep 2\");
    println!(\"keep 3\");
    println!(\"keep 4\");
    println!(\"keep 5\");
    println!(\"keep 6\");
    println!(\"keep 7\");
    println!(\"keep 8\");
}

pub fn omega(y: i32) -> i32 {
    let b = y * 999;
    b
}
";
    fs::write(root.join("src/lib.rs"), updated).unwrap();
    fs::write(root.join("notes.md"), "# New note\nHello\n").unwrap();

    let st = load_status(root, home, 10).unwrap();
    assert_eq!(st.unstaged.len(), 2);
    assert_eq!(st.staged.len(), 0);
    assert_eq!(st.commits.len(), 1);

    let folds = HashSet::new();
    let diff = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "unstaged",
        path: "src/lib.rs",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();

    if crate::git::find_omnidiff_bin().is_some() {
        assert_eq!(diff.engine, "omnidiff");
        assert_eq!(diff.language.as_deref(), Some("Rust"));
    }
    assert_eq!(diff.chunks.len(), 2, "expected 2 independent chunks");

    let c0 = &diff.chunks[0];
    stage_chunk(
        root,
        "unstaged",
        "src/lib.rs",
        None,
        c0.b_start,
        c0.b_end,
        c0.a_start,
        c0.a_end,
        Some(&c0.signature),
    )
    .unwrap();

    let st_after_stage = load_status(root, home, 5).unwrap();
    assert_eq!(st_after_stage.staged.len(), 1);
    assert_eq!(st_after_stage.staged[0].path, "src/lib.rs");

    let staged_diff = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "staged",
        path: "src/lib.rs",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    assert_eq!(staged_diff.chunks.len(), 1);

    let unstaged_diff = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "unstaged",
        path: "src/lib.rs",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    assert_eq!(unstaged_diff.chunks.len(), 1);

    let sc0 = &staged_diff.chunks[0];
    unstage_chunk(
        root,
        "staged",
        "src/lib.rs",
        None,
        sc0.b_start,
        sc0.b_end,
        sc0.a_start,
        sc0.a_end,
        Some(&sc0.signature),
    )
    .unwrap();
    let st_after_unstage = load_status(root, home, 5).unwrap();
    assert_eq!(st_after_unstage.staged.len(), 0);

    let untracked_diff = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "unstaged",
        path: "notes.md",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    assert_eq!(untracked_diff.chunks.len(), 1);
    let uc = &untracked_diff.chunks[0];
    stage_chunk(
        root,
        "unstaged",
        "notes.md",
        None,
        uc.b_start,
        uc.b_end,
        uc.a_start,
        uc.a_end,
        Some(&uc.signature),
    )
    .unwrap();
    assert_eq!(load_status(root, home, 5).unwrap().staged.len(), 1);

    let fresh_diff = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "src/lib.rs",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    let rc1 = &fresh_diff.chunks[1];
    let err = revert_chunk(
        root,
        home,
        "all",
        "src/lib.rs",
        None,
        rc1.b_start,
        rc1.b_end,
        rc1.a_start,
        rc1.a_end,
        Some("0000deadbeef"),
        Some(&rc1.signature),
    )
    .unwrap_err();
    assert!(err.to_string().contains("modified concurrently"));

    let real_hash = hex_sha256(updated.as_bytes());
    revert_chunk(
        root,
        home,
        "all",
        "src/lib.rs",
        None,
        rc1.b_start,
        rc1.b_end,
        rc1.a_start,
        rc1.a_end,
        Some(&real_hash),
        Some(&rc1.signature),
    )
    .unwrap();
    let on_disk = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(on_disk.contains("x + 100"));
    assert!(on_disk.contains("y * 2"));

    let undone = pop_undo(root, home).unwrap();
    assert_eq!(undone.path, "src/lib.rs");
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), updated);

    let toggled = toggle_review(
        root,
        home,
        "src/lib.rs",
        Some(&fresh_diff.chunks[0].signature),
        None,
    )
    .unwrap();
    assert!(toggled);

    discard_file(root, home, "all", "src/lib.rs").unwrap();
    assert!(fs::read_to_string(root.join("src/lib.rs")).unwrap().contains("x + 1;"));
    pop_undo(root, home).unwrap();
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), updated);
}

#[test]
fn rex_installer_stamps_lua_updates_init_and_uninstalls_cleanly() {
    let rex_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let home = rex_home.path();

    fs::write(home.join("init.lua"), "-- user custom line\n").unwrap();

    let repo_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let installed_bin =
        crate::installer::install_with_bin_dir(repo_dir, home, bin_dir.path(), true).unwrap();
    assert!(installed_bin.to_string_lossy().contains("rex-omnidiff"));

    let stamped_lua = fs::read_to_string(home.join("omnidiff.lua")).unwrap();
    assert!(!stamped_lua.contains("@REX_OMNIDIFF_BIN@"));
    assert!(stamped_lua.contains("omnidiff_toggle"));
    assert!(stamped_lua.contains("omnidiff_dock"));
    assert!(stamped_lua.contains("shift+cmd+g"));
    assert!(stamped_lua.contains("alt+cmd+g"));

    let init_text = fs::read_to_string(home.join("init.lua")).unwrap();
    assert!(init_text.contains("-- user custom line"));
    assert!(init_text.contains("require(\"omnidiff\")"));

    // Idempotent second install does not duplicate require("omnidiff")
    crate::installer::install_with_bin_dir(repo_dir, home, bin_dir.path(), true).unwrap();
    let init_after_second = fs::read_to_string(home.join("init.lua")).unwrap();
    assert_eq!(init_after_second.matches("require(\"omnidiff\")").count(), 1);

    crate::installer::uninstall(home, true).unwrap();
    assert!(!home.join("omnidiff.lua").exists());
    assert!(!home.join("data/omnidiff").exists());
    let init_after_uninstall = fs::read_to_string(home.join("init.lua")).unwrap();
    assert!(init_after_uninstall.contains("-- user custom line"));
    assert!(!init_after_uninstall.contains("require(\"omnidiff\")"));
}

#[test]
fn scope_all_adjacent_partial_deleted_untracked_and_renamed_staging() {
    let (repo, app_home) = init_sample_repo();
    let root = repo.path();
    let home = app_home.path();
    let folds = HashSet::new();

    let base_swift = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
    fs::write(root.join("FlowMainView.swift"), base_swift).unwrap();
    git(root, &["add", "FlowMainView.swift"]);
    git(root, &["commit", "-m", "Add FlowMainView.swift"]);

    let v1_swift = "line 1\nline 2\nline 3\nline 4 staged v1\nline 5\nline 6\n";
    fs::write(root.join("FlowMainView.swift"), v1_swift).unwrap();
    git(root, &["add", "FlowMainView.swift"]);

    let v2_swift = "line 1\nline 2 edited\nline 3\nline 4 edited v2\nline 5\nline 6\n";
    fs::write(root.join("FlowMainView.swift"), v2_swift).unwrap();

    let d_all = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "FlowMainView.swift",
        old_path: None,
        commit_hash: None,
        preset: "minimal",
        context_lines: 3,
        expanded_folds: &folds,
    })
    .unwrap();
    assert_eq!(d_all.chunks.len(), 2, "expected 2 adjacent chunks 1 line apart");
    assert!(!d_all.chunks[0].staged);
    assert!(!d_all.chunks[1].staged);

    let c0 = &d_all.chunks[0];
    stage_chunk(
        root,
        "all",
        "FlowMainView.swift",
        None,
        c0.b_start,
        c0.b_end,
        c0.a_start,
        c0.a_end,
        Some(&c0.signature),
    )
    .unwrap();

    stage_chunk(
        root,
        "all",
        "FlowMainView.swift",
        None,
        c0.b_start,
        c0.b_end,
        c0.a_start,
        c0.a_end,
        Some(&c0.signature),
    )
    .unwrap();

    let d_mid = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "FlowMainView.swift",
        old_path: None,
        commit_hash: None,
        preset: "minimal",
        context_lines: 3,
        expanded_folds: &folds,
    })
    .unwrap();
    assert!(d_mid.chunks[0].staged);
    assert!(!d_mid.chunks[1].staged);

    let c1 = &d_mid.chunks[1];
    stage_chunk(
        root,
        "all",
        "FlowMainView.swift",
        None,
        c1.b_start,
        c1.b_end,
        c1.a_start,
        c1.a_end,
        Some(&c1.signature),
    )
    .unwrap();

    let d_both = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "FlowMainView.swift",
        old_path: None,
        commit_hash: None,
        preset: "minimal",
        context_lines: 3,
        expanded_folds: &folds,
    })
    .unwrap();
    assert!(d_both.chunks.iter().all(|c| c.staged));
    assert_eq!(load_status(root, home, 5).unwrap().unstaged.len(), 0);

    let c0_staged = &d_both.chunks[0];
    unstage_chunk(
        root,
        "all",
        "FlowMainView.swift",
        None,
        c0_staged.b_start,
        c0_staged.b_end,
        c0_staged.a_start,
        c0_staged.a_end,
        Some(&c0_staged.signature),
    )
    .unwrap();

    let d_after_unstage = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "FlowMainView.swift",
        old_path: None,
        commit_hash: None,
        preset: "minimal",
        context_lines: 3,
        expanded_folds: &folds,
    })
    .unwrap();
    assert!(!d_after_unstage.chunks[0].staged);
    assert!(d_after_unstage.chunks[1].staged);

    fs::write(root.join("brand_new.txt"), "hello\nworld\n").unwrap();
    let d_new = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "brand_new.txt",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    let nc = &d_new.chunks[0];
    stage_chunk(
        root,
        "all",
        "brand_new.txt",
        None,
        nc.b_start,
        nc.b_end,
        nc.a_start,
        nc.a_end,
        Some(&nc.signature),
    )
    .unwrap();
    unstage_chunk(
        root,
        "all",
        "brand_new.txt",
        None,
        nc.b_start,
        nc.b_end,
        nc.a_start,
        nc.a_end,
        Some(&nc.signature),
    )
    .unwrap();
    let st_untracked_restored = load_status(root, home, 5).unwrap();
    assert!(
        !st_untracked_restored
            .staged
            .iter()
            .any(|f| f.path == "brand_new.txt")
    );
    fs::remove_file(root.join("brand_new.txt")).unwrap();

    fs::remove_file(root.join("src/lib.rs")).unwrap();
    let d_del = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "src/lib.rs",
        old_path: None,
        commit_hash: None,
        preset: "default",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    let dc = &d_del.chunks[0];
    stage_chunk(
        root,
        "all",
        "src/lib.rs",
        None,
        dc.b_start,
        dc.b_end,
        dc.a_start,
        dc.a_end,
        Some(&dc.signature),
    )
    .unwrap();
    let st_del = load_status(root, home, 5).unwrap();
    assert!(
        st_del
            .staged
            .iter()
            .any(|f| f.path == "src/lib.rs" && f.status == "D")
    );
    unstage_chunk(
        root,
        "all",
        "src/lib.rs",
        None,
        dc.b_start,
        dc.b_end,
        dc.a_start,
        dc.a_end,
        Some(&dc.signature),
    )
    .unwrap();
    git(root, &["checkout", "HEAD", "--", "src/lib.rs"]);

    git(root, &["checkout", "HEAD", "--", "FlowMainView.swift"]);
    git(root, &["mv", "FlowMainView.swift", "RenamedView.swift"]);
    fs::write(
        root.join("RenamedView.swift"),
        "line 1\nline 2 renamed edit\nline 3\nline 4\nline 5\nline 6\n",
    )
    .unwrap();
    let d_ren = compute_diff(&DiffParams {
        cwd: root,
        app_home: home,
        scope: "all",
        path: "RenamedView.swift",
        old_path: Some("FlowMainView.swift"),
        commit_hash: None,
        preset: "minimal",
        context_lines: 2,
        expanded_folds: &folds,
    })
    .unwrap();
    let rc = &d_ren.chunks[0];
    stage_chunk(
        root,
        "all",
        "RenamedView.swift",
        Some("FlowMainView.swift"),
        rc.b_start,
        rc.b_end,
        rc.a_start,
        rc.a_end,
        Some(&rc.signature),
    )
    .unwrap();
    unstage_file(root, "RenamedView.swift", Some("FlowMainView.swift")).unwrap();
    assert_eq!(load_status(root, home, 5).unwrap().staged.len(), 0);
    stage_file(root, "RenamedView.swift", Some("FlowMainView.swift")).unwrap();
    assert!(
        load_status(root, home, 5)
            .unwrap()
            .staged
            .iter()
            .any(|f| f.path == "RenamedView.swift" && f.status == "R")
    );
}

#[test]
fn combined_raw_numstat_parser_and_ast_cache() {
    let raw = [
        b":100644 100644 4cb29ea f384549 R073".as_slice(),
        b"a.txt",
        b"a_ren.txt",
        b":000000 100644 0000000 0000000 U",
        b"conflict.rs",
        b":100644 100644 ba2906d 0000000 M",
        b"conflict.rs",
        b":100644 100644 8352675 0000000 M",
        b":weird.bin",
        b"1\t0\t",
        b"a.txt",
        b"a_ren.txt",
        b"0\t0\tconflict.rs",
        b"4\t2\tconflict.rs",
        b"-\t-\t:weird.bin",
        b"",
    ]
    .join(&0u8);
    let (items, stats) = crate::git::parse_raw_and_numstat_z(&raw);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].status, "R");
    assert_eq!(items[0].path, "a_ren.txt");
    assert_eq!(items[0].old_path.as_deref(), Some("a.txt"));
    assert_eq!(items[1].status, "U");
    assert_eq!(items[1].path, "conflict.rs");
    assert_eq!(items[2].status, "M");
    assert_eq!(items[2].path, ":weird.bin");
    assert_eq!(stats["a_ren.txt"].add, 1);
    assert_eq!(stats["conflict.rs"].add, 4);
    assert_eq!(stats["conflict.rs"].del, 2);
    assert!(stats[":weird.bin"].binary);
}

#[test]
fn kitty_graphics_protocol_compositor_and_escape_encoding_work() {
    let mut before = RgbaImage::from_pixel(32, 32, Rgba([20, 30, 40, 255]));
    let mut after = before.clone();
    for y in 8..16 {
        for x in 10..20 {
            after.put_pixel(x, y, Rgba([240, 80, 120, 255]));
        }
    }
    before.put_pixel(0, 0, Rgba([20, 30, 40, 255]));

    let stats = compute_image_diff_stats(Some(&before), Some(&after));
    assert_eq!(stats.before_dims, Some((32, 32)));
    assert_eq!(stats.after_dims, Some((32, 32)));
    assert_eq!(stats.changed_pixels, 80);
    assert!(!stats.regions.is_empty());

    for mode in [
        KittyImageMode::SideBySide,
        KittyImageMode::DiffHeatmap,
        KittyImageMode::SplitWipe,
        KittyImageMode::OnionBlend,
    ] {
        let comp = render_composite_diff_image(
            Some(&before),
            Some(&after),
            &stats,
            mode,
            50,
            true,
            240,
            120,
        );
        assert_eq!(comp.width(), 240);
        assert_eq!(comp.height(), 120);
        let esc = encode_kitty_png_escape(&comp, 5, 4, 60, 18).unwrap();
        assert!(esc.contains(&format!("\x1b_Ga=d,d=I,i={KITTY_IMAGE_ID},q=2\x1b\\")));
        assert!(esc.contains("\x1b[4;5H"));
        assert!(esc.contains(&format!("\x1b_Ga=T,f=100,i={KITTY_IMAGE_ID},c=60,r=18,C=1,q=2")));
    }
}

#[test]
fn rex_agent_discovery_parses_osc7501_and_process_metadata() {
    assert_eq!(
        parse_file_url_path("file://MacBook-Pro.local/Users/verte/personal_code"),
        Some("/Users/verte/personal_code".to_string())
    );

    let proc_json = json!({
        "foreground": {
            "name": "claude",
            "cwd": "/Users/verte/personal_code/projects/rex-omnidiff"
        }
    });
    let blocked_status = json!({
        "records": [
            {
                "app": "claude",
                "state": "blocked",
                "kind": "permission"
            }
        ]
    });
    let choice = classify_block_agent(
        "block:01a1d1b7-857c-7518-b54b-8031059bf25b",
        Some("window:w1"),
        Some(&proc_json),
        Some(&blocked_status),
    )
    .expect("should classify Claude Code block");

    assert_eq!(choice.program_key, "claude");
    assert_eq!(choice.program_name, "Claude Code");
    assert_eq!(choice.status, "blocked");
    assert_eq!(choice.blocked_kind.as_deref(), Some("permission"));

    let ready_status = json!({
        "records": [
            {
                "effective_app": "agy",
                "state": "idle"
            }
        ]
    });
    let agy_choice = classify_block_agent(
        "block:01a1d1a8-49ee-7e48-bff7-fc406c3b2586",
        Some("window:w1"),
        None,
        Some(&ready_status),
    )
    .expect("should classify Antigravity block");
    assert_eq!(agy_choice.program_key, "antigravity");
    assert_eq!(agy_choice.status, "ready");
}

#[test]
fn rex_bundle_theme_loader_and_tui_palette_generation_work() {
    let sample_bundle = r##"[
      {
        "id": "velvet-dusk",
        "name": "Velvet Dusk",
        "appearance": "dark",
        "source": {
          "background": "#16161e",
          "foreground": "#c8d3f5",
          "ansi": [
            "#1b1d2b", "#ff757f", "#c3e88d", "#ffc777",
            "#82aaff", "#c099ff", "#86e1fc", "#c8d3f5"
          ]
        }
      }
    ]"##;
    let raw = load_theme_from_bundle_json(sample_bundle, "velvet-dusk").unwrap();
    assert_eq!(raw.name, "Velvet Dusk");
    assert!(raw.is_dark);

    let pal = build_palette(&raw);
    assert_eq!(pal.theme_id, "velvet-dusk");
    assert!(pal.tui_custom.insert_bg.starts_with('#'));

    let mut st = LineSyntaxState::default();
    let tokens = tokenize_syntax_segment("pub fn hello(x: u32) // comment", &pal, &mut st, false);
    assert!(tokens.len() >= 4);

    let (repo, app) = init_sample_repo();
    let palette_json = serde_json::to_string(& build_palette(&RawTheme::default()).tui_custom).unwrap();
    let spec = crate::actions::prepare_tui_launch(
        repo.path(),
        app.path(),
        "all",
        None,
        None,
        None,
        "minimal",
        Some(&palette_json),
    )
    .unwrap();
    let cfg_path = app.path().join("data").join("omnidiff").join("tui-config.toml");
    assert!(cfg_path.is_file());
    #[cfg(unix)]
    assert!(spec.argv.iter().any(|a| a.starts_with("OMNIDIFF_CONFIG=")));
}
