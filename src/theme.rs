use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use ratatui::style::Color;
use serde::Deserialize;

use crate::actions::TuiCustomPalette;

#[derive(Debug, Clone)]
pub struct RawTheme {
    pub id: String,
    pub name: String,
    pub is_dark: bool,
    pub background: String,
    pub foreground: String,
    pub surface: String,
    pub raised: String,
    pub border: String,
    pub muted: String,
    pub dim: String,
    pub accent: String,
    pub ansi1: String, // red
    pub ansi2: String, // green
    pub ansi3: String, // yellow
    pub ansi4: String, // blue
    pub ansi5: String, // magenta/purple
    pub ansi6: String, // cyan
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ThemePalette {
    pub theme_id: String,
    pub theme_name: String,
    pub is_dark: bool,
    pub bg: Color,
    pub fg: Color,
    pub surface: Color,
    pub raised: Color,
    pub chrome: Color,
    pub border: Color,
    pub muted: Color,
    pub dim: Color,
    pub accent: Color,
    pub add_text: Color,
    pub del_text: Color,
    pub move_text: Color,
    pub upd_text: Color,
    pub ask_text: Color,
    pub warn_text: Color,
    pub ask_bg: Color,
    pub ask_row_bg: Color,
    pub queued_row_bg: Color,
    pub seg_i: Color,
    pub seg_d: Color,
    pub seg_u: Color,
    pub seg_m: Color,
    pub row_i: Color,
    pub row_d: Color,
    pub row_u: Color,
    pub row_m: Color,
    pub empty_split: Color,
    pub chunk_bg: Color,
    pub chunk_active_bg: Color,
    pub chunk_border: Color,
    pub chunk_active_border: Color,
    pub chunk_range_text: Color,
    pub fold_bg: Color,
    pub staged_bg: Color,
    pub reviewed_bg: Color,
    pub selected_file_bg: Color,
    // Syntax token colors
    pub syn_keyword: Color,
    pub syn_string: Color,
    pub syn_number: Color,
    pub syn_type: Color,
    pub syn_fn: Color,
    pub syn_comment: Color,
    pub syn_punct: Color,
    pub tui_custom: TuiCustomPalette,
}

pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let raw = hex.strip_prefix('#')?;
    match raw.len() {
        3 | 4 => {
            let r = u8::from_str_radix(&raw[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&raw[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&raw[2..3].repeat(2), 16).ok()?;
            Some((r, g, b))
        }
        6 | 8 => {
            let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
            let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
            let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

pub fn to_hex6(hex: &str, fallback: &str) -> String {
    if let Some((r, g, b)) = parse_hex_rgb(hex) {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else if let Some((r, g, b)) = parse_hex_rgb(fallback) {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        "#888888".to_string()
    }
}

pub fn mix_hex(base_hex: &str, tint_hex: &str, weight: f32) -> String {
    let a = parse_hex_rgb(base_hex);
    let b = parse_hex_rgb(tint_hex);
    match (a, b) {
        (None, None) => "#000000".to_string(),
        (None, Some(_)) => to_hex6(tint_hex, "#000000"),
        (Some(_), None) => to_hex6(base_hex, "#000000"),
        (Some((ar, ag, ab)), Some((br, bg, bb))) => {
            let w = weight.clamp(0.0, 1.0);
            let r = ((ar as f32) * (1.0 - w) + (br as f32) * w).round() as u8;
            let g = ((ag as f32) * (1.0 - w) + (bg as f32) * w).round() as u8;
            let bl = ((ab as f32) * (1.0 - w) + (bb as f32) * w).round() as u8;
            format!("#{r:02x}{g:02x}{bl:02x}")
        }
    }
}

pub fn hex_to_color(hex: &str) -> Color {
    let (r, g, b) = parse_hex_rgb(hex).unwrap_or((136, 136, 136));
    Color::Rgb(r, g, b)
}

impl Default for RawTheme {
    fn default() -> Self {
        Self::velvet_dusk()
    }
}

impl RawTheme {
    pub fn velvet_dusk() -> Self {
        let bg = "#16161e";
        let fg = "#c8d3f5";
        Self {
            id: "velvet-dusk".to_string(),
            name: "Velvet Dusk".to_string(),
            is_dark: true,
            background: bg.to_string(),
            foreground: fg.to_string(),
            surface: mix_hex(bg, "#ffffff", 0.04),
            raised: mix_hex(bg, "#ffffff", 0.08),
            border: mix_hex(bg, fg, 0.16),
            muted: mix_hex(bg, fg, 0.62),
            dim: mix_hex(bg, fg, 0.42),
            accent: "#82aaff".to_string(),
            ansi1: "#ff757f".to_string(),
            ansi2: "#c3e88d".to_string(),
            ansi3: "#ffc777".to_string(),
            ansi4: "#82aaff".to_string(),
            ansi5: "#c099ff".to_string(),
            ansi6: "#86e1fc".to_string(),
        }
    }

    pub fn merino_light() -> Self {
        let bg = "#f7f5f0";
        let fg = "#1f2335";
        Self {
            id: "merino-light".to_string(),
            name: "Merino Light".to_string(),
            is_dark: false,
            background: bg.to_string(),
            foreground: fg.to_string(),
            surface: mix_hex(bg, "#000000", 0.035),
            raised: mix_hex(bg, "#000000", 0.07),
            border: mix_hex(bg, fg, 0.16),
            muted: mix_hex(bg, fg, 0.62),
            dim: mix_hex(bg, fg, 0.44),
            accent: "#2e7de9".to_string(),
            ansi1: "#f52a65".to_string(),
            ansi2: "#587539".to_string(),
            ansi3: "#8c6c3e".to_string(),
            ansi4: "#2e7de9".to_string(),
            ansi5: "#9854f1".to_string(),
            ansi6: "#007197".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RexBundleTheme {
    id: String,
    name: String,
    appearance: String,
    source: RexBundleThemeSource,
}

#[derive(Debug, Deserialize)]
struct RexBundleThemeSource {
    background: String,
    foreground: String,
    #[serde(default)]
    ansi: Vec<String>,
}

fn read_macos_default(key: &str) -> Option<String> {
    let out = Command::new("defaults")
        .args(["read", "com.superlogical.rex", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn detect_rex_active_theme_id() -> (String, bool) {
    if let Ok(explicit) = env::var("REX_OMNIDIFF_THEME")
        && !explicit.is_empty()
    {
        let is_dark = !explicit.contains("light") && !explicit.contains("paper");
        return (explicit, is_dark);
    }
    let appearance = read_macos_default("Workspace.appearance").unwrap_or_else(|| "dark".into());
    let is_dark = appearance != "light";
    let theme_id = if is_dark {
        read_macos_default("Workspace.darkThemeID").unwrap_or_else(|| "velvet-dusk".into())
    } else {
        read_macos_default("Workspace.lightThemeID").unwrap_or_else(|| "merino-light".into())
    };
    (theme_id, is_dark)
}

pub fn load_theme_from_bundle_json(json_str: &str, target_id: &str) -> Option<RawTheme> {
    let themes: Vec<RexBundleTheme> = serde_json::from_str(json_str).ok()?;
    let found = themes
        .into_iter()
        .find(|t| t.id.eq_ignore_ascii_case(target_id))?;
    let is_dark = found.appearance != "light";
    let bg = to_hex6(&found.source.background, if is_dark { "#16161e" } else { "#f7f5f0" });
    let fg = to_hex6(&found.source.foreground, if is_dark { "#c8d3f5" } else { "#1f2335" });
    let pole = if is_dark { "#ffffff" } else { "#000000" };
    let ansi = &found.source.ansi;
    let get_ansi = |idx: usize, fallback: &str| -> String {
        ansi.get(idx)
            .map(|s| to_hex6(s, fallback))
            .unwrap_or_else(|| fallback.to_string())
    };
    let ansi4 = get_ansi(4, "#82aaff");
    Some(RawTheme {
        id: found.id,
        name: found.name,
        is_dark,
        surface: mix_hex(&bg, pole, if is_dark { 0.04 } else { 0.035 }),
        raised: mix_hex(&bg, pole, if is_dark { 0.08 } else { 0.07 }),
        border: mix_hex(&bg, &fg, 0.16),
        muted: mix_hex(&bg, &fg, 0.62),
        dim: mix_hex(&bg, &fg, 0.42),
        accent: ansi4.clone(),
        ansi1: get_ansi(1, "#ff757f"),
        ansi2: get_ansi(2, "#c3e88d"),
        ansi3: get_ansi(3, "#ffc777"),
        ansi4,
        ansi5: get_ansi(5, "#c099ff"),
        ansi6: get_ansi(6, "#86e1fc"),
        background: bg,
        foreground: fg,
    })
}

pub fn load_active_rex_palette() -> ThemePalette {
    let (theme_id, is_dark) = detect_rex_active_theme_id();
    let bundle_candidates = [
        "/Applications/Rex Beta.app/Contents/Resources/SuperUI_SuperDesignTokens.bundle/Contents/Resources/themes.json",
        "/Applications/Rex.app/Contents/Resources/SuperUI_SuperDesignTokens.bundle/Contents/Resources/themes.json",
    ];
    for cand in bundle_candidates {
        let p = Path::new(cand);
        if p.is_file()
            && let Ok(text) = fs::read_to_string(p)
            && let Some(raw) = load_theme_from_bundle_json(&text, &theme_id)
        {
            return build_palette(&raw);
        }
    }
    let fallback = if is_dark {
        RawTheme::velvet_dusk()
    } else {
        RawTheme::merino_light()
    };
    build_palette(&fallback)
}

pub fn build_palette(t: &RawTheme) -> ThemePalette {
    let is_dark = t.is_dark;
    let bg = &t.background;
    let surface = &t.surface;
    let accent = &t.accent;

    let add_accent = mix_hex(&t.ansi2, &t.ansi6, if is_dark { 0.34 } else { 0.16 });
    let del_accent = to_hex6(&t.ansi1, "#f7768e");
    let move_accent = to_hex6(&t.ansi5, "#bb9af7");
    let upd_accent = to_hex6(accent, "#7aa2f7");
    let ask_accent = to_hex6(&t.ansi6, "#7dcfff");

    let seg_i = mix_hex(bg, &add_accent, if is_dark { 0.34 } else { 0.28 });
    let seg_d = mix_hex(bg, &del_accent, if is_dark { 0.32 } else { 0.26 });
    let seg_u = mix_hex(bg, &upd_accent, if is_dark { 0.32 } else { 0.24 });
    let seg_m = mix_hex(bg, &move_accent, if is_dark { 0.27 } else { 0.20 });

    let row_i = mix_hex(bg, &add_accent, if is_dark { 0.14 } else { 0.12 });
    let row_d = mix_hex(bg, &del_accent, if is_dark { 0.13 } else { 0.11 });
    let row_u = mix_hex(bg, &upd_accent, if is_dark { 0.13 } else { 0.11 });
    let row_m = mix_hex(bg, &move_accent, if is_dark { 0.12 } else { 0.10 });

    let raised_base = mix_hex(
        &mix_hex(bg, &t.raised, 0.50),
        if is_dark { "#ffffff" } else { "#000000" },
        if is_dark { 0.05 } else { 0.03 },
    );
    let chunk_bg = mix_hex(&raised_base, accent, if is_dark { 0.17 } else { 0.13 });
    let chunk_active_bg = mix_hex(&raised_base, accent, if is_dark { 0.29 } else { 0.23 });
    let chunk_border = mix_hex(&t.border, accent, if is_dark { 0.44 } else { 0.38 });
    let chunk_active_border = mix_hex(&t.border, accent, if is_dark { 0.80 } else { 0.72 });
    let chunk_range_text = mix_hex(&t.foreground, &ask_accent, if is_dark { 0.42 } else { 0.34 });

    let chrome = mix_hex(bg, surface, 0.75);
    let fold_bg = mix_hex(bg, "#000000", if is_dark { 0.24 } else { 0.045 });
    let empty_split = mix_hex(bg, "#000000", if is_dark { 0.22 } else { 0.055 });
    let staged_bg = mix_hex(surface, &add_accent, 0.22);
    let reviewed_bg = mix_hex(surface, &t.ansi6, 0.22);
    let selected_file_bg = mix_hex(surface, accent, if is_dark { 0.24 } else { 0.18 });
    let ask_bg = mix_hex(surface, &ask_accent, if is_dark { 0.20 } else { 0.15 });
    let ask_row_bg = mix_hex(bg, &ask_accent, if is_dark { 0.18 } else { 0.14 });
    let queued_row_bg = mix_hex(bg, &move_accent, if is_dark { 0.14 } else { 0.10 });

    let tui_custom = TuiCustomPalette {
        insert_bg: seg_i.clone(),
        delete_bg: seg_d.clone(),
        move_bg: seg_m.clone(),
        update_bg: seg_u.clone(),
        overlay_fg: to_hex6(&t.foreground, "#c0caf5"),
        cross_highlight_bg: mix_hex(bg, accent, if is_dark { 0.38 } else { 0.30 }),
        search_bg: mix_hex(bg, &t.ansi3, if is_dark { 0.36 } else { 0.28 }),
        before_title_fg: del_accent.clone(),
        after_title_fg: add_accent.clone(),
    };

    ThemePalette {
        theme_id: t.id.clone(),
        theme_name: t.name.clone(),
        is_dark,
        bg: hex_to_color(bg),
        fg: hex_to_color(&t.foreground),
        surface: hex_to_color(surface),
        raised: hex_to_color(&t.raised),
        chrome: hex_to_color(&chrome),
        border: hex_to_color(&t.border),
        muted: hex_to_color(&t.muted),
        dim: hex_to_color(&t.dim),
        accent: hex_to_color(accent),
        add_text: hex_to_color(&add_accent),
        del_text: hex_to_color(&del_accent),
        move_text: hex_to_color(&move_accent),
        upd_text: hex_to_color(&upd_accent),
        ask_text: hex_to_color(&ask_accent),
        warn_text: hex_to_color(&t.ansi3),
        ask_bg: hex_to_color(&ask_bg),
        ask_row_bg: hex_to_color(&ask_row_bg),
        queued_row_bg: hex_to_color(&queued_row_bg),
        seg_i: hex_to_color(&seg_i),
        seg_d: hex_to_color(&seg_d),
        seg_u: hex_to_color(&seg_u),
        seg_m: hex_to_color(&seg_m),
        row_i: hex_to_color(&row_i),
        row_d: hex_to_color(&row_d),
        row_u: hex_to_color(&row_u),
        row_m: hex_to_color(&row_m),
        empty_split: hex_to_color(&empty_split),
        chunk_bg: hex_to_color(&chunk_bg),
        chunk_active_bg: hex_to_color(&chunk_active_bg),
        chunk_border: hex_to_color(&chunk_border),
        chunk_active_border: hex_to_color(&chunk_active_border),
        chunk_range_text: hex_to_color(&chunk_range_text),
        fold_bg: hex_to_color(&fold_bg),
        staged_bg: hex_to_color(&staged_bg),
        reviewed_bg: hex_to_color(&reviewed_bg),
        selected_file_bg: hex_to_color(&selected_file_bg),
        syn_keyword: hex_to_color(&t.ansi5),
        syn_string: hex_to_color(&t.ansi2),
        syn_number: hex_to_color(&t.ansi3),
        syn_type: hex_to_color(&t.ansi6),
        syn_fn: hex_to_color(&t.ansi4),
        syn_comment: hex_to_color(&t.dim),
        syn_punct: hex_to_color(&t.muted),
        tui_custom,
    }
}

// ---- Fast Lightweight Syntax Tokenizer for Diff Segments --------------------

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "crate", "def",
    "default", "defer", "delete", "do", "dyn", "elif", "else", "enum", "export", "extends",
    "extern", "finally", "fn", "for", "from", "func", "function", "go", "guard", "if", "impl",
    "import", "in", "interface", "let", "loop", "match", "mod", "move", "mut", "namespace", "new",
    "override", "package", "priv", "private", "protected", "protocol", "pub", "public", "raise",
    "readonly", "ref", "return", "select", "static", "struct", "super", "switch", "throw",
    "throws", "trait", "try", "type", "typeof", "unsafe", "use", "var", "where", "while", "with",
    "yield", "local", "then", "end", "elseif", "not", "and", "or",
];

const LITERALS: &[&str] = &[
    "true", "false", "True", "False", "null", "nil", "None", "Some", "Ok", "Err", "undefined",
    "NaN", "Infinity", "self", "Self", "this",
];

const PRIMITIVES: &[&str] = &[
    "bool", "boolean", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8",
    "u16", "u32", "u64", "u128", "usize", "str", "string", "number", "void", "any", "never",
    "unknown", "object", "symbol", "bigint", "int", "float",
];

#[derive(Debug, Default, Clone)]
pub struct LineSyntaxState {
    pub in_comment: bool,
    pub seen_non_space: bool,
}

pub fn tokenize_syntax_segment(
    text: &str,
    pal: &ThemePalette,
    state: &mut LineSyntaxState,
    on_highlight_bg: bool,
) -> Vec<(String, Color)> {
    if text.is_empty() {
        return Vec::new();
    }
    if state.in_comment {
        return vec![(text.to_string(), pal.syn_comment)];
    }

    let punct_fg = if on_highlight_bg { pal.fg } else { pal.syn_punct };
    let mut out: Vec<(String, Color)> = Vec::new();
    let mut push_run = |s: &str, fg: Color| {
        if s.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.1 == fg
        {
            last.0.push_str(s);
        } else {
            out.push((s.to_string(), fg));
        }
    };

    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        let ch = text[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if !ch.is_whitespace() && !state.seen_non_space {
            state.seen_non_space = true;
            if ch == '#' && !text[i..].starts_with("#[") && !text[i..].starts_with("#![") {
                state.in_comment = true;
                push_run(&text[i..], pal.syn_comment);
                break;
            }
        }

        // Line comments `//` or `-- `
        if text[i..].starts_with("//") || text[i..].starts_with("-- ") {
            state.in_comment = true;
            push_run(&text[i..], pal.syn_comment);
            break;
        }

        // String literal
        if ch == '"' || ch == '`' {
            let quote = ch;
            let start = i;
            i += ch_len;
            while i < len {
                let c = text[i..].chars().next().unwrap();
                i += c.len_utf8();
                if c == '\\' && i < len {
                    let next_c = text[i..].chars().next().unwrap();
                    i += next_c.len_utf8();
                } else if c == quote {
                    break;
                }
            }
            push_run(&text[start..i], pal.syn_string);
            continue;
        }

        // Identifier or keyword
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += ch_len;
            while i < len {
                let c = text[i..].chars().next().unwrap();
                if c.is_ascii_alphanumeric() || c == '_' {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
            if i < len && bytes[i] == b'!' {
                i += 1;
            }
            let ident = &text[start..i];
            let next_non_ws = text[i..].trim_start().chars().next();
            let fg = if ident.ends_with('!') {
                pal.syn_fn
            } else if KEYWORDS.contains(&ident) {
                pal.syn_keyword
            } else if LITERALS.contains(&ident) {
                pal.syn_number
            } else if PRIMITIVES.contains(&ident)
                || (ident.starts_with(|c: char| c.is_ascii_uppercase())
                    && ident.chars().any(|c| c.is_ascii_lowercase()))
            {
                pal.syn_type
            } else if next_non_ws == Some('(') {
                pal.syn_fn
            } else {
                pal.fg
            };
            push_run(ident, fg);
            continue;
        }

        // Numbers
        if ch.is_ascii_digit() {
            let start = i;
            i += ch_len;
            while i < len {
                let c = text[i..].chars().next().unwrap();
                if c.is_ascii_hexdigit() || matches!(c, '_' | '.' | 'x' | 'b' | 'o') {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
            push_run(&text[start..i], pal.syn_number);
            continue;
        }

        let fg = if ch.is_whitespace() { pal.fg } else { punct_fg };
        push_run(&text[i..(i + ch_len)], fg);
        i += ch_len;
    }

    out
}
