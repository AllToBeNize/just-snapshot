use std::cell::Cell;

/// Color theme shared by the capture toolbar, option bar, settings and dialogs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Theme {
    #[default]
    Graphite,
    Midnight,
    Forest,
    Light,
}

pub const THEMES: [Theme; 4] = [Theme::Graphite, Theme::Midnight, Theme::Forest, Theme::Light];

/// All values are COLORREF (0x00BBGGRR).
pub struct Palette {
    pub background: u32,
    pub card: u32,
    pub card_border: u32,
    pub divider: u32,
    pub text: u32,
    pub secondary: u32,
    pub tertiary: u32,
    pub icon: u32,
    pub disabled: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub accent_soft: u32,
    pub accent_muted: u32,
    pub on_accent: u32,
    pub success: u32,
    pub warning: u32,
    pub warning_soft: u32,
    pub error: u32,
    pub hover: u32,
    pub pressed: u32,
    pub key_face: u32,
    pub key_border: u32,
    pub key_depth: u32,
    pub track_off: u32,
    pub shadow: u32,
    pub tooltip: u32,
    pub tooltip_text: u32,
}

pub const fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

const GRAPHITE: Palette = Palette {
    background: rgb(0x1c, 0x1d, 0x20),
    card: rgb(0x26, 0x27, 0x2b),
    card_border: rgb(0x36, 0x38, 0x3d),
    divider: rgb(0x31, 0x33, 0x38),
    text: rgb(0xec, 0xed, 0xef),
    secondary: rgb(0xa4, 0xa8, 0xaf),
    tertiary: rgb(0x70, 0x75, 0x7d),
    icon: rgb(0xcb, 0xce, 0xd3),
    disabled: rgb(0x5a, 0x5e, 0x66),
    accent: rgb(0x5e, 0x8b, 0xf0),
    accent_hover: rgb(0x78, 0x9f, 0xf4),
    accent_soft: rgb(0x2b, 0x35, 0x4e),
    accent_muted: rgb(0x3a, 0x46, 0x63),
    on_accent: rgb(0xff, 0xff, 0xff),
    success: rgb(0x5c, 0xbf, 0x83),
    warning: rgb(0xe2, 0xa2, 0x3b),
    warning_soft: rgb(0x3b, 0x31, 0x22),
    error: rgb(0xe5, 0x67, 0x5f),
    hover: rgb(0x31, 0x33, 0x38),
    pressed: rgb(0x3b, 0x3d, 0x43),
    key_face: rgb(0x30, 0x32, 0x36),
    key_border: rgb(0x46, 0x49, 0x4f),
    key_depth: rgb(0x15, 0x16, 0x18),
    track_off: rgb(0x4a, 0x4d, 0x54),
    shadow: rgb(0x0e, 0x0f, 0x11),
    tooltip: rgb(0x0f, 0x10, 0x12),
    tooltip_text: rgb(0xf2, 0xf2, 0xf2),
};

const MIDNIGHT: Palette = Palette {
    background: rgb(0x13, 0x18, 0x26),
    card: rgb(0x1b, 0x22, 0x33),
    card_border: rgb(0x2b, 0x34, 0x4a),
    divider: rgb(0x26, 0x2e, 0x42),
    text: rgb(0xe7, 0xeb, 0xf3),
    secondary: rgb(0x9a, 0xa5, 0xbc),
    tertiary: rgb(0x66, 0x71, 0x8a),
    icon: rgb(0xc3, 0xcb, 0xdb),
    disabled: rgb(0x4e, 0x58, 0x6e),
    accent: rgb(0x6e, 0x9b, 0xff),
    accent_hover: rgb(0x8a, 0xb0, 0xff),
    accent_soft: rgb(0x23, 0x32, 0x52),
    accent_muted: rgb(0x33, 0x41, 0x5f),
    on_accent: rgb(0xff, 0xff, 0xff),
    success: rgb(0x5c, 0xc3, 0x9a),
    warning: rgb(0xe8, 0xa8, 0x4a),
    warning_soft: rgb(0x38, 0x30, 0x24),
    error: rgb(0xef, 0x6b, 0x6b),
    hover: rgb(0x24, 0x2c, 0x40),
    pressed: rgb(0x2d, 0x36, 0x51),
    key_face: rgb(0x23, 0x2b, 0x3e),
    key_border: rgb(0x37, 0x41, 0x5b),
    key_depth: rgb(0x0b, 0x0f, 0x18),
    track_off: rgb(0x3d, 0x47, 0x60),
    shadow: rgb(0x08, 0x0b, 0x12),
    tooltip: rgb(0x0a, 0x0e, 0x17),
    tooltip_text: rgb(0xee, 0xf1, 0xf7),
};

const FOREST: Palette = Palette {
    background: rgb(0x17, 0x1d, 0x1b),
    card: rgb(0x1f, 0x27, 0x24),
    card_border: rgb(0x2f, 0x3a, 0x36),
    divider: rgb(0x2a, 0x34, 0x30),
    text: rgb(0xe6, 0xee, 0xea),
    secondary: rgb(0x9d, 0xb0, 0xa8),
    tertiary: rgb(0x67, 0x79, 0x70),
    icon: rgb(0xc2, 0xd1, 0xca),
    disabled: rgb(0x52, 0x61, 0x5a),
    accent: rgb(0x3f, 0xb5, 0x8e),
    accent_hover: rgb(0x5a, 0xc7, 0xa1),
    accent_soft: rgb(0x1f, 0x3a, 0x31),
    accent_muted: rgb(0x2d, 0x4b, 0x40),
    on_accent: rgb(0xff, 0xff, 0xff),
    success: rgb(0x5c, 0xc3, 0x9a),
    warning: rgb(0xe2, 0xa2, 0x3b),
    warning_soft: rgb(0x3a, 0x32, 0x22),
    error: rgb(0xe5, 0x67, 0x5f),
    hover: rgb(0x27, 0x31, 0x2d),
    pressed: rgb(0x31, 0x3d, 0x38),
    key_face: rgb(0x25, 0x30, 0x2c),
    key_border: rgb(0x39, 0x47, 0x41),
    key_depth: rgb(0x0c, 0x10, 0x0e),
    track_off: rgb(0x3f, 0x4c, 0x47),
    shadow: rgb(0x09, 0x0c, 0x0b),
    tooltip: rgb(0x0c, 0x10, 0x0e),
    tooltip_text: rgb(0xee, 0xf3, 0xf0),
};

const LIGHT: Palette = Palette {
    background: rgb(0xf3, 0xf4, 0xf6),
    card: rgb(0xff, 0xff, 0xff),
    card_border: rgb(0xe0, 0xe3, 0xe7),
    divider: rgb(0xec, 0xee, 0xf1),
    text: rgb(0x1f, 0x23, 0x29),
    secondary: rgb(0x6b, 0x72, 0x80),
    tertiary: rgb(0x9a, 0xa0, 0xa8),
    icon: rgb(0x45, 0x4b, 0x54),
    disabled: rgb(0xb9, 0xb9, 0xb9),
    accent: rgb(0x3d, 0x6f, 0xd9),
    accent_hover: rgb(0x2f, 0x5b, 0xbd),
    accent_soft: rgb(0xe6, 0xee, 0xfc),
    accent_muted: rgb(0xbf, 0xcf, 0xf0),
    on_accent: rgb(0xff, 0xff, 0xff),
    success: rgb(0x3a, 0x93, 0x5e),
    warning: rgb(0xc4, 0x7d, 0x1a),
    warning_soft: rgb(0xfd, 0xf4, 0xe4),
    error: rgb(0xc9, 0x45, 0x45),
    hover: rgb(0xf0, 0xf2, 0xf5),
    pressed: rgb(0xe4, 0xe7, 0xeb),
    key_face: rgb(0xf8, 0xf9, 0xfb),
    key_border: rgb(0xd5, 0xd9, 0xdf),
    key_depth: rgb(0xc9, 0xce, 0xd5),
    track_off: rgb(0xc9, 0xcd, 0xd4),
    shadow: rgb(0xb4, 0xb8, 0xbe),
    tooltip: rgb(0x2d, 0x30, 0x34),
    tooltip_text: rgb(0xff, 0xff, 0xff),
};

impl Theme {
    pub fn key(self) -> &'static str {
        match self {
            Self::Graphite => "graphite",
            Self::Midnight => "midnight",
            Self::Forest => "forest",
            Self::Light => "light",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Graphite => "石墨",
            Self::Midnight => "午夜蓝",
            Self::Forest => "墨绿",
            Self::Light => "浅色",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        THEMES.into_iter().find(|theme| theme.key() == value.trim())
    }

    pub fn palette(self) -> &'static Palette {
        match self {
            Self::Graphite => &GRAPHITE,
            Self::Midnight => &MIDNIGHT,
            Self::Forest => &FOREST,
            Self::Light => &LIGHT,
        }
    }
}

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(Theme::Graphite) };
}

pub fn current() -> Theme {
    CURRENT.get()
}

pub fn set(theme: Theme) {
    CURRENT.set(theme);
}

pub fn palette() -> &'static Palette {
    current().palette()
}

/// Linear blend between two COLORREF values.
pub fn mix(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    [0, 8, 16].into_iter().fold(0, |color, shift| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        color | (((a + (b - a) * t).round() as u32) << shift)
    })
}

/// COLORREF plus alpha to GDI+ ARGB.
pub fn argb(color: u32, alpha: u8) -> u32 {
    (u32::from(alpha) << 24) | ((color & 0xff) << 16) | (color & 0xff00) | ((color >> 16) & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_round_trip_and_keep_text_readable() {
        let luminance = |color: u32| {
            let channel = |shift: u32| ((color >> shift) & 0xff) as f32 / 255.0;
            0.2126 * channel(0) + 0.7152 * channel(8) + 0.0722 * channel(16)
        };
        for theme in THEMES {
            assert_eq!(Theme::parse(theme.key()), Some(theme));
            let palette = theme.palette();
            let contrast = (luminance(palette.text) - luminance(palette.card)).abs();
            assert!(contrast > 0.6, "{theme:?} text contrast {contrast}");
        }
        assert_eq!(Theme::default(), Theme::Graphite, "dark by default");
        assert_eq!(Theme::parse("unknown"), None);
    }
}
