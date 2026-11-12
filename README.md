# rex-omnidiff

**Structural AST & Kitty Graphics Protocol Diff Workspace for [Rex](https://superlogical.com)**

`rex-omnidiff` brings the full structural AST diff, hunk-staging, undo-journal, and AI-agent review workflow of [`herdr-omnidiff`](https://github.com/verte21/herdr-omnidiff) natively into **Rex** — supercharged with **Ghostty 24-bit TrueColor**, **SGR mouse hitboxes**, **OSC 7501 agent awareness**, and **inline Kitty Graphics Protocol (`\x1b_G...`)** image diffing.

---

## Highlights

- **Dual Rex Launch Modes (Bound Out of the Box)**
  - **`Shift+Cmd+G` (`omnidiff_toggle`)** — Toggles a centered `94% × 90%` floating overlay layer (`omnidiff-float`) over your active Rex window, rooted at the focused terminal's Git repository. Press `Shift+Cmd+G`, `q`, or `Esc` to dismiss and return focus to your terminal.
  - **`Alt+Cmd+G` (`omnidiff_dock`)** — Toggles a persistent right-hand docked split (`omnidiff-dock`, `ratio = 0.64`) beside your active terminal and automatically follows repository changes as you switch panes.
- **Tree-sitter Structural AST Diffs (Powered by [`omnidiff`](https://github.com/dirmacs/omnidiff))**
  - Intra-line token highlights for **Insertions (`+`)**, **Deletions (`-`)**, **Updates (`~`)**, and **Moved code blocks (`⇄`)** across 25+ languages, with automatic Patience-diff fallback.
  - **2-Column Side-by-Side Split & Unified Views** (`Tab` to toggle, auto-adaptive by terminal width) with horizontal scroll (`h`/`l`, `←`/`→`, `0`/`$`, `Shift+Wheel`), soft wrap (`w`), clickable context folds (`⋯ N unchanged lines ⋯`), and enclosing `@ function / struct` scope breadcrumbs.
  - **Cross-Linked Move Navigation (`m`)** — Jump directly between the origin and destination hunks of moved functions or blocks.
- **Inline High-DPI Image Diffs via Kitty Graphics Protocol (`\x1b_G...`)**
  - Renders true pixel-level visual diffs directly inside the Rex terminal canvas for `.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, `.bmp`, and `.ico` changes:
    1. **Side-by-Side** — Scaled Before & After canvases over an alpha checkerboard with changed-region bounding boxes.
    2. **Pixel Heatmap** — Highlights changed pixels by RGBΔ intensity and clusters modified regions into cyan bounding boxes (`x` to toggle boxes).
    3. **Interactive Split Wipe** — Vertical wipe bar between Before and After (`←`/`→` or click `◄`/`►` to scrub 0–100%).
    4. **Onion-Skin Alpha Blend** — Variable opacity blend between Before and After (`←`/`→` to adjust blend percentage).
- **Surgical Hunk & File Staging + 25-Deep Undo Journal**
  - Stage (`s`), unstage (`u`), or revert (`x`) individual hunks across `All (HEAD)`, `Unstaged`, and `Staged` scopes — including adjacent hunks 1 line apart, partially staged files, untracked files, deletions, and renames.
  - Every destructive action (hunk revert or file discard) snapshots the working-tree file into `~/.config/rex/data/omnidiff/undo/` before touching disk. Press **`z`** (or click `↶ Undo (N)`) to restore content and file permissions.
- **OSC 7501 Agent-Aware "Ask AI" Code Review (`a`)**
  - Click any diff line (`Shift+Click` for a multi-line range) or press **`a`** on the active hunk to open the inline AI review composer.
  - Automatically discovers live coding agents running across your Rex session (`Antigravity`, `Claude Code`, `Codex`, `OpenCode`, `Pi`, `Aider`, `Gemini CLI`, `Amp`) via `rex block call ... process` and **OSC 7501 `program_status`**.
  - Respects agent state (`ready`, `working`, `blocked` on permission/question/auth — never force-sends into a blocked agent prompt), supports multi-comment batch queueing (`Ctrl+Q`), or spawns a new agent split on demand.
- **Live Rex Theme Synchronization**
  - Reads Rex's active appearance (`Workspace.appearance`, `Workspace.darkThemeID`, `Workspace.lightThemeID`) and `/Applications/Rex Beta.app/.../themes.json` to derive all 24-bit TrueColor surface, syntax, and diff tokens — and writes a matching `tui-config.toml` when launching native `omnidiff` (`t`).

---

## Quick Install

### Prerequisites

1. **Rex** (`Rex Beta.app` or `Rex.app` with the `rex` CLI on `PATH`)
2. **Rust toolchain** (`cargo`)
3. **`omnidiff` CLI** (`>= 0.3.0` recommended):
   ```bash
   brew install dirmacs/tap/omnidiff
   # or: cargo install omnidiff
   ```

### One-Command Build & Install

```bash
git clone https://github.com/verte21/rex-omnidiff.git
cd rex-omnidiff
./install.sh
```

What `./install.sh` does:
1. Builds `target/release/rex-omnidiff` and installs it to `~/.local/bin/rex-omnidiff`.
2. Installs `~/.config/rex/omnidiff.lua` and adds `require("omnidiff")` to `~/.config/rex/init.lua` (preserving all existing configuration).
3. Runs `rex config check` and `rex config reload` so `Shift+Cmd+G` and `Alt+Cmd+G` are active immediately.

### Uninstall

```bash
rex-omnidiff uninstall
```

---

## Keyboard & Mouse Reference

| Shortcut | Action |
| :--- | :--- |
| **`Shift+Cmd+G`** | Toggle floating `rex-omnidiff` overlay in active Rex window (`omnidiff_toggle`) |
| **`Alt+Cmd+G`** | Toggle docked right-hand `rex-omnidiff` split in active Rex window (`omnidiff_dock`) |
| **`1` / `2` / `3` / `4`** | Switch scope: `All` / `Unstaged` / `Staged` / `Commits` |
| **`j` / `k`** | Select next / previous file in sidebar |
| **`J` / `K`** or **`↓` / `↑`** | Scroll diff viewport vertically |
| **`h` / `l`** or **`←` / `→`** | Scroll code horizontally (or scrub Kitty image wipe/blend %) |
| **`0` / `$`** | Jump to start / end of longest line horizontally |
| **`n` / `p`** or **`]` / `[`** | Step to next / previous diff hunk |
| **`m`** | Jump to counterpart moved hunk (`⇄`) |
| **`s` / `u` / `x`** | Stage / Unstage / Revert active hunk (`x` toggles bounding boxes on images) |
| **`Space`** | Stage / Unstage selected file |
| **`S` / `U`** | Stage All / Unstage All files |
| **`v`** | Toggle `✓ Reviewed` badge on file (persisted by content SHA-256) |
| **`z`** | Pop Undo journal (restores last reverted hunk or discarded file) |
| **`a`** or **Click Line** | Open **Ask AI** review composer on selected line range or active hunk |
| **`c`** | Open **Commit** message bar (`Enter` = commit staged, `Ctrl+A` = stage all & commit) |
| **`i`** | Cycle Kitty Graphics image diff mode (`Side-by-Side` → `Heatmap` → `Split Wipe` → `Onion Blend`) |
| **`Tab`** | Toggle `Split` (2-column side-by-side) vs `Unified` diff view |
| **`w`** | Toggle soft line wrapping |
| **`F`** | Cycle AST diff preset (`Auto` → `Minimal` → `Full`) |
| **`b`** | Toggle left file rail |
| **`t`** | Launch native `omnidiff` TUI with synced Rex theme palette |
| **`r`** | Refresh Git status & AST diff |
| **`q` / `Esc`** | Close `rex-omnidiff` overlay or pane |

---

## License

[MIT](./LICENSE)
