//! Theme palettes and terminal background detection.
//!
//! Every widget in `render.rs` draws with the standard ANSI palette (`Color::Cyan`,
//! `Color::DarkGray`, …). After the frame is drawn, `Palette::map_fg`/`map_bg` remap those
//! ANSI colors (and `Color::Reset`) to explicit RGB values for the active theme + contrast,
//! so the app looks identical in every terminal regardless of its palette.

use ratatui::style::Color;

use crate::app::{Background, Contrast};

/// The resolved RGB colors for one theme + contrast combination. Fields are semantic roles;
/// `map_fg`/`map_bg` translate the ANSI colors used at draw time into these.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Base background (also what `Color::Reset` backgrounds resolve to).
    pub bg: Color,
    /// Base foreground (also what `Color::Reset` foregrounds resolve to).
    pub fg: Color,
    /// Selection / elevated-surface background (`Color::DarkGray` backgrounds).
    pub selection_bg: Color,
    /// Modal drop-shadow background (`Color::Black` backgrounds).
    pub shadow: Color,
    /// Primary accent — focused borders, links, active chips (`Color::Cyan`).
    pub accent: Color,
    /// Success (`Color::Green`).
    pub ok: Color,
    /// Warning / in-progress (`Color::Yellow`).
    pub warn: Color,
    /// Error (`Color::Red`).
    pub error: Color,
    /// Secondary accent (`Color::Magenta`).
    pub info: Color,
    /// Tertiary accent (`Color::Blue`, mostly from git's own ANSI output).
    pub blue: Color,
    /// Secondary text (`Color::Gray`).
    pub muted: Color,
    /// Tertiary text, dim borders (`Color::DarkGray` foregrounds).
    pub faint: Color,
    /// Strongest text (`Color::White`).
    pub bright: Color,
    /// Deep accent surface (`Color::LightCyan`) — active chip backgrounds. Dark enough in the
    /// light themes that inverse (background-colored) text keeps a high contrast ratio, so
    /// terminals with minimum-contrast enforcement don't repaint it black.
    pub accent_deep: Color,
}

impl Palette {
    /// Remap a foreground color drawn with the ANSI palette to this palette's RGB value.
    /// RGB/indexed colors pass through untouched.
    pub fn map_fg(&self, color: Color) -> Color {
        match color {
            Color::Reset => self.fg,
            Color::Cyan => self.accent,
            Color::LightCyan => self.accent_deep,
            Color::Green | Color::LightGreen => self.ok,
            Color::Yellow | Color::LightYellow => self.warn,
            Color::Red | Color::LightRed => self.error,
            Color::Magenta | Color::LightMagenta => self.info,
            Color::Blue | Color::LightBlue => self.blue,
            Color::Gray => self.muted,
            Color::DarkGray => self.faint,
            Color::White => self.bright,
            // Inverse text on accent-colored chips reads as the base background.
            Color::Black => self.bg,
            other => other,
        }
    }

    /// Remap a background color drawn with the ANSI palette to this palette's RGB value.
    pub fn map_bg(&self, color: Color) -> Color {
        match color {
            Color::Reset => self.bg,
            Color::DarkGray => self.selection_bg,
            Color::Black => self.shadow,
            other => self.map_fg(other),
        }
    }
}

static DARK_NORMAL: Palette = Palette {
    bg: Color::Rgb(26, 27, 38),
    fg: Color::Rgb(192, 197, 206),
    selection_bg: Color::Rgb(59, 66, 97),
    shadow: Color::Rgb(16, 16, 24),
    accent: Color::Rgb(125, 207, 255),
    ok: Color::Rgb(158, 206, 106),
    warn: Color::Rgb(224, 175, 104),
    error: Color::Rgb(247, 118, 142),
    info: Color::Rgb(187, 154, 247),
    blue: Color::Rgb(122, 162, 247),
    muted: Color::Rgb(160, 168, 189),
    faint: Color::Rgb(86, 95, 137),
    bright: Color::Rgb(230, 233, 240),
    accent_deep: Color::Rgb(125, 207, 255),
};

static DARK_SOFT: Palette = Palette {
    bg: Color::Rgb(35, 37, 48),
    fg: Color::Rgb(170, 176, 189),
    selection_bg: Color::Rgb(62, 66, 84),
    shadow: Color::Rgb(27, 29, 38),
    accent: Color::Rgb(108, 178, 209),
    ok: Color::Rgb(143, 174, 115),
    warn: Color::Rgb(201, 167, 109),
    error: Color::Rgb(211, 128, 143),
    info: Color::Rgb(169, 149, 214),
    blue: Color::Rgb(126, 152, 207),
    muted: Color::Rgb(139, 146, 168),
    faint: Color::Rgb(92, 99, 120),
    bright: Color::Rgb(201, 205, 217),
    accent_deep: Color::Rgb(108, 178, 209),
};

static LIGHT_NORMAL: Palette = Palette {
    bg: Color::Rgb(245, 246, 248),
    fg: Color::Rgb(40, 42, 48),
    selection_bg: Color::Rgb(212, 214, 228),
    shadow: Color::Rgb(200, 202, 212),
    accent: Color::Rgb(0, 138, 173),
    ok: Color::Rgb(26, 127, 55),
    warn: Color::Rgb(140, 108, 62),
    error: Color::Rgb(207, 34, 46),
    info: Color::Rgb(120, 71, 189),
    blue: Color::Rgb(59, 130, 246),
    muted: Color::Rgb(94, 100, 115),
    faint: Color::Rgb(157, 163, 178),
    bright: Color::Rgb(14, 16, 20),
    accent_deep: Color::Rgb(0, 96, 122),
};

static LIGHT_SOFT: Palette = Palette {
    bg: Color::Rgb(235, 237, 240),
    fg: Color::Rgb(75, 80, 92),
    selection_bg: Color::Rgb(215, 218, 226),
    shadow: Color::Rgb(197, 200, 209),
    accent: Color::Rgb(38, 148, 178),
    ok: Color::Rgb(45, 138, 75),
    warn: Color::Rgb(153, 121, 74),
    error: Color::Rgb(196, 66, 78),
    info: Color::Rgb(138, 104, 184),
    blue: Color::Rgb(86, 138, 222),
    muted: Color::Rgb(111, 116, 128),
    faint: Color::Rgb(166, 171, 182),
    bright: Color::Rgb(42, 46, 56),
    accent_deep: Color::Rgb(31, 104, 126),
};

/// Blend `color` toward `toward` by `amount` (0.0 = unchanged, 1.0 = fully `toward`).
/// Non-RGB colors pass through unchanged. Used to materialize DIM cells — terminals render
/// the DIM attribute inconsistently (often not at all over truecolor), so fading the color
/// itself is the only reliable way to show a disabled/no-op state.
pub fn blend_toward(color: Color, toward: Color, amount: f32) -> Color {
    let (Color::Rgb(red, green, blue), Color::Rgb(to_red, to_green, to_blue)) = (color, toward)
    else {
        return color;
    };
    let mix = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
    };
    Color::Rgb(mix(red, to_red), mix(green, to_green), mix(blue, to_blue))
}

/// The base (normal vs soft) palette for a resolved dark/light mode — the source for one axis.
fn base(dark: bool, soft: bool) -> &'static Palette {
    match (dark, soft) {
        (true, false) => &DARK_NORMAL,
        (true, true) => &DARK_SOFT,
        (false, false) => &LIGHT_NORMAL,
        (false, true) => &LIGHT_SOFT,
    }
}

/// Compose a palette from two independent axes: `Background` picks the surface tones
/// (`bg`/`selection_bg`/`shadow`), `Contrast` picks the text + accent + semantic colors.
/// When both axes agree this reproduces one of the four static palettes exactly. `Terminal`
/// leaves `bg` as `Color::Reset` so the terminal's own background shows through (the row-selection
/// and modal-shadow surfaces still get a tone, from the normal base, so they stay visible).
pub fn palette(dark: bool, background: Background, contrast: Contrast) -> Palette {
    let bg_src = base(dark, background == Background::Soft);
    let fg_src = base(dark, contrast == Contrast::Soft);
    Palette {
        bg: if background == Background::Terminal { Color::Reset } else { bg_src.bg },
        selection_bg: bg_src.selection_bg,
        shadow: bg_src.shadow,
        ..*fg_src
    }
}

/// Detect whether the terminal background is dark, for the Auto theme. Tries, in order:
/// an OSC 11 query of the terminal itself, the `COLORFGBG` env var, the Windows light/dark
/// setting via `reg.exe` under WSL, the macOS appearance setting — then defaults to dark.
///
/// Must be called BEFORE the TUI enters raw mode / the alternate screen (the OSC query
/// manages raw mode itself and reads the reply from the tty).
pub fn detect_dark_background() -> bool {
    use terminal_colorsaurus::{theme_mode, QueryOptions, ThemeMode};
    if let Ok(mode) = theme_mode(QueryOptions::default()) {
        return mode == ThemeMode::Dark;
    }
    if let Some(dark) = std::env::var("COLORFGBG").ok().and_then(|raw| colorfgbg_dark(&raw)) {
        return dark;
    }
    if let Some(dark) = wsl_windows_dark() {
        return dark;
    }
    if let Some(dark) = macos_dark() {
        return dark;
    }
    true
}

/// Re-detect dark/light at RUNTIME using only the tty-safe sources — `COLORFGBG`, the WSL
/// Windows light/dark registry value, and the macOS appearance — skipping the OSC 11 query
/// (which manages raw mode + reads the tty, so it can't run while the event loop owns stdin).
/// Returns `None` when none apply (terminal reports its background only via OSC). Lets the Auto
/// theme follow an OS light↔dark switch live, without restarting. Cheap; safe to poll.
pub fn detect_dark_background_runtime() -> Option<bool> {
    if let Some(dark) = std::env::var("COLORFGBG").ok().and_then(|raw| colorfgbg_dark(&raw)) {
        return Some(dark);
    }
    if let Some(dark) = wsl_windows_dark() {
        return Some(dark);
    }
    macos_dark()
}

/// Parse a `COLORFGBG` value ("15;0" or "15;default;0") — the last segment is the background
/// color index: 0-6 and 8 are dark, 7 and 9-15 are light.
fn colorfgbg_dark(raw: &str) -> Option<bool> {
    let bg: u8 = raw.rsplit(';').next()?.trim().parse().ok()?;
    Some(bg <= 6 || bg == 8)
}

/// Under WSL, read the Windows "apps use light theme" registry value via `reg.exe`.
/// Terminals that follow the system theme (Tabby "From system", Windows Terminal default)
/// track exactly this value — and most of them don't answer OSC 11.
fn wsl_windows_dark() -> Option<bool> {
    if std::env::var_os("WSL_DISTRO_NAME").is_none() && std::env::var_os("WSL_INTEROP").is_none() {
        return None;
    }
    let output = std::process::Command::new("/mnt/c/Windows/System32/reg.exe")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
        .ok()?;
    reg_output_dark(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `reg.exe query` output: `AppsUseLightTheme REG_DWORD 0x1` → light (not dark).
fn reg_output_dark(output: &str) -> Option<bool> {
    let line = output.lines().find(|line| line.contains("AppsUseLightTheme"))?;
    let value = line.split_whitespace().last()?;
    match value {
        "0x0" => Some(true),
        "0x1" => Some(false),
        _ => None,
    }
}

/// On macOS, `defaults read -g AppleInterfaceStyle` prints "Dark" in dark mode and errors
/// (key absent) in light mode.
fn macos_dark() -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .ok()?;
    Some(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("Dark"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_two_part_dark() {
        assert_eq!(colorfgbg_dark("15;0"), Some(true));
    }

    #[test]
    fn colorfgbg_two_part_light() {
        assert_eq!(colorfgbg_dark("0;15"), Some(false));
    }

    #[test]
    fn colorfgbg_three_part() {
        assert_eq!(colorfgbg_dark("15;default;0"), Some(true));
    }

    #[test]
    fn colorfgbg_garbage() {
        assert_eq!(colorfgbg_dark("default;default"), None);
        assert_eq!(colorfgbg_dark(""), None);
    }

    #[test]
    fn reg_light_theme() {
        let output = "\r\nHKEY_CURRENT_USER\\...\\Personalize\r\n    AppsUseLightTheme    REG_DWORD    0x1\r\n";
        assert_eq!(reg_output_dark(output), Some(false));
    }

    #[test]
    fn reg_dark_theme() {
        let output = "    AppsUseLightTheme    REG_DWORD    0x0";
        assert_eq!(reg_output_dark(output), Some(true));
    }

    #[test]
    fn reg_missing_value() {
        assert_eq!(reg_output_dark("ERROR: The system was unable to find the specified key"), None);
    }

    #[test]
    fn blend_moves_rgb_toward_target() {
        assert_eq!(
            blend_toward(Color::Rgb(0, 0, 0), Color::Rgb(100, 100, 100), 0.5),
            Color::Rgb(50, 50, 50)
        );
        assert_eq!(blend_toward(Color::Indexed(3), Color::Rgb(0, 0, 0), 0.5), Color::Indexed(3));
    }

    #[test]
    fn map_bg_distinguishes_selection_from_faint_text() {
        let palette = palette(true, Background::Normal, Contrast::Normal);
        assert_ne!(palette.map_bg(Color::DarkGray), palette.map_fg(Color::DarkGray));
        assert_eq!(palette.map_bg(Color::Reset), palette.bg);
    }

    #[test]
    fn palette_composes_two_axes() {
        // Soft background + normal contrast: surface from DARK_SOFT, text/accent from DARK_NORMAL.
        let mixed = palette(true, Background::Soft, Contrast::Normal);
        assert_eq!(mixed.bg, DARK_SOFT.bg);
        assert_eq!(mixed.selection_bg, DARK_SOFT.selection_bg);
        assert_eq!(mixed.shadow, DARK_SOFT.shadow);
        assert_eq!(mixed.fg, DARK_NORMAL.fg);
        assert_eq!(mixed.accent, DARK_NORMAL.accent);
        assert_eq!(mixed.ok, DARK_NORMAL.ok);
    }

    #[test]
    fn palette_axis_equal_matches_static() {
        // When both axes agree, the composed palette reproduces the legacy static one.
        let composed = palette(false, Background::Soft, Contrast::Soft);
        assert_eq!(composed.bg, LIGHT_SOFT.bg);
        assert_eq!(composed.fg, LIGHT_SOFT.fg);
        assert_eq!(composed.error, LIGHT_SOFT.error);
    }
}
