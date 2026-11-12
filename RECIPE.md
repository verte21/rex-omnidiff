# rex-omnidiff Architecture & Recipe

This document explains how `rex-omnidiff` integrates with **Rex** (`com.superlogical.rex`), how its Lua action layer communicates with the Rust TUI and RPC engine, and how it leverages Ghostty's **Kitty Graphics Protocol** and **OSC 7501** program status reporting.

---

## 1. Architectural Overview

```
┌──────────────────────────────────────────────────────────────────────────┐
│ Rex Window (macOS)                                                       │
│  ├─ ~/.config/rex/init.lua ──► require("omnidiff")                       │
│  │    ├─ Action: omnidiff_toggle (Shift+Cmd+G) -> rex.session.open_layer │
│  │    └─ Action: omnidiff_dock   (Alt+Cmd+G)   -> rex.window.split       │
│  │                                                                       │
│  └─ Ghostty Terminal Block (`com.superlogical.terminal.shell`)           │
│       └─ ~/.local/bin/rex-omnidiff ui --mode float|dock ...              │
│            ├─ Ratatui + Crossterm 24-bit TrueColor Canvas + SGR Mouse    │
│            ├─ Kitty Graphics Protocol (`\x1b_Ga=T,f=100,i=4242...`)      │
│            ├─ Tree-sitter AST Diff Engine (`omnidiff --mode json`)       │
│            ├─ Patience Line-Splicing Index Engine (`git hash-object -w`) │
│            └─ Rex Session Bridge (`rex session inspect`, `rex send`)     │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Module Breakdown

| File | Responsibility |
| :--- | :--- |
| [`lua/omnidiff.lua`](./lua/omnidiff.lua) | Registers `omnidiff_toggle` (`shift+cmd+g`) and `omnidiff_dock` (`alt+cmd+g`) in Rex's Lua runtime, inspects the focused terminal's foreground CWD via `rex.block.call("com.superlogical.terminal", "process", ...)`, and spawns or closes the floating layer / docked split. |
| [`src/main.rs`](./src/main.rs) | CLI entrypoint supporting `ui`, `install`, `uninstall`, and JSON `api` subcommands (`status`, `diff`, `stage-chunk`, `unstage-chunk`, `revert-chunk`, `undo`, `review-toggle`, `commit`, `prepare-tui`). |
| [`src/ui.rs`](./src/ui.rs) | Interactive Ratatui + Crossterm TUI with full mouse hitbox routing, Unified & 2-column Side-by-Side Split diff views, inline Ask AI & Commit composers, live repo following, and post-draw Kitty image emission. |
| [`src/kitty.rs`](./src/kitty.rs) | Loads before/after images (`png`, `jpg`, `gif`, `webp`, `bmp`, `ico`), computes pixel deltas and 8×8-clustered bounding boxes, composites **Side-by-Side**, **Pixel Heatmap**, **Split Wipe**, and **Onion Blend** canvases, and encodes chunked Kitty Graphics Protocol (`\x1b_G...`) PNG streams. |
| [`src/rex_bridge.rs`](./src/rex_bridge.rs) | Queries Rex sessions (`rex session inspect`, `rex block call ... process` / `program_status` / `pwd`), classifies live coding agents with OSC 7501 state (`ready`, `working`, `blocked`), and dispatches structured review prompts via `rex send` + `rex send-key Enter` or `rex split`. |
| [`src/theme.rs`](./src/theme.rs) | Reads macOS defaults (`com.superlogical.rex`) and `/Applications/Rex Beta.app/.../themes.json` to build a 24-bit TrueColor `ThemePalette` + fast syntax tokenizer + `tui-config.toml` generator. |
| [`src/diff_engine.rs`](./src/diff_engine.rs) | Runs `omnidiff --mode json` with an LRU disk cache (`scratch/ast-cache/`), slices UTF-8 + tab-expanded lines by Tree-sitter byte spans, cross-links moved blocks (`⇄`), and builds Patience-aligned Unified and Split row models. |
| [`src/actions.rs`](./src/actions.rs) | Implements line-range splicing (`splice_line_range`) + 3-way `git merge-file` fallback + direct `git hash-object -w --stdin` / `git update-index --cacheinfo` hunk staging, optimistic SHA-256 concurrency checks, and the 25-entry Undo journal. |
| [`src/git.rs`](./src/git.rs) | Runs parallel lock-free `git diff --raw --numstat -M -z` status queries (`std::thread::scope`), materializes before/after scratch pairs, and stages image media for Kitty previews. |
| [`src/installer.rs`](./src/installer.rs) | Atomically installs `~/.local/bin/rex-omnidiff`, stamps `~/.config/rex/omnidiff.lua`, updates `~/.config/rex/init.lua`, and runs `rex config check` + `rex config reload`. |
