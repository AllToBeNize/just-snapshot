//! The translation panel beside a pin made with "translate": a small toolbar
//! (target language, copy, settings) over the translated text, with a
//! skeleton while waiting and the full error when something fails.

use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::*,
};

use crate::{
    gdiplus::Graphics,
    icons::{self, Icon},
    selection_geometry::contains,
    theme::{self, argb, mix},
    translate::{Language, Provider},
    ui::{self, Fonts},
};

const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
const TOOLBAR: i32 = 44;
const CONTROL: i32 = 28;
const PAD: i32 = 14;
const ACTIONS: i32 = 44;
/// Width of the panel relative to the image, within these bounds.
pub const MIN_WIDTH: i32 = 300;
pub const MAX_WIDTH: i32 = 520;
pub const MIN_HEIGHT: i32 = 240;
const SHIMMER_MS: f32 = 1300.0;

pub enum Phase {
    /// Text recognition is still running.
    Recognizing,
    Translating { to: Language, provider: Option<Provider> },
    Done { text: String, to: Language },
    NoText,
    Unconfigured,
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Language,
    Copy,
    Settings,
    /// "设置翻译服务" when unconfigured, "重试" after a failure.
    Primary,
    /// "翻译设置" after a failure.
    Secondary,
}

pub struct TranslatePane {
    pub language: Language,
    pub phase: Phase,
    /// Identifies the request in flight; older replies are dropped.
    pub request: u32,
    pub scroll: i32,
    pub hovered: Option<Target>,
    pub pressed: Option<Target>,
}

pub struct Layout {
    scale: f32,
    pub panel: RECT,
    toolbar: RECT,
    language: RECT,
    copy: Option<RECT>,
    settings: RECT,
    /// Where text scrolls.
    text: RECT,
    primary: Option<RECT>,
    secondary: Option<RECT>,
}

impl Layout {
    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    /// The language button, where its menu drops down from.
    pub fn language(&self) -> RECT {
        self.language
    }

    fn targets(&self) -> impl Iterator<Item = (Target, RECT)> + '_ {
        [
            (Target::Language, Some(self.language)),
            (Target::Copy, self.copy),
            (Target::Settings, Some(self.settings)),
            (Target::Primary, self.primary),
            (Target::Secondary, self.secondary),
        ]
        .into_iter()
        .filter_map(|(target, rect)| Some((target, rect?)))
    }
}

const COPY_LABEL: &str = "复制译文";
const SETUP_LABEL: &str = "设置翻译服务";
const RETRY_LABEL: &str = "重试";
const SETTINGS_LABEL: &str = "翻译设置";

impl TranslatePane {
    pub fn new() -> Self {
        Self {
            language: Language::Auto,
            phase: Phase::Recognizing,
            request: 0,
            scroll: 0,
            hovered: None,
            pressed: None,
        }
    }

    pub fn translation(&self) -> Option<&str> {
        match &self.phase {
            Phase::Done { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Recognizing | Phase::Translating { .. })
    }

    /// "自动 · English" once auto has picked a language.
    pub fn language_label(&self) -> String {
        let resolved = match self.phase {
            Phase::Translating { to, .. } | Phase::Done { to, .. } => Some(to),
            _ => None,
        };
        match (self.language, resolved) {
            (Language::Auto, Some(to)) => format!("自动 · {}", to.label()),
            (language, _) => language.label().to_string(),
        }
    }

    pub unsafe fn layout(&self, panel: RECT, scale: f32, dc: HDC, fonts: &Fonts) -> Layout {
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let toolbar = RECT { bottom: panel.top + s(TOOLBAR), ..panel };
        let control_top = toolbar.top + (s(TOOLBAR) - s(CONTROL)) / 2;
        let control = |left: i32, width: i32| RECT {
            left,
            top: control_top,
            right: left + width,
            bottom: control_top + s(CONTROL),
        };
        // Icon, label and chevron.
        let label = ui::measure(dc, &self.language_label(), fonts.small);
        let language = control(panel.left + s(8), s(8 + 16 + 6) + label + s(4 + 14 + 6));
        let settings = control(panel.right - s(8) - s(CONTROL), s(CONTROL));
        let copy = self.translation().map(|_| {
            let width = ui::measure(dc, COPY_LABEL, fonts.small) + s(20);
            control(settings.left - s(6) - width, width)
        });
        let body = RECT {
            left: panel.left + s(PAD),
            top: toolbar.bottom + s(10),
            right: panel.right - s(PAD),
            bottom: panel.bottom - s(10),
        };
        let button = |label: &str| ui::measure(dc, label, fonts.small) + s(28);
        let actions_top = body.bottom - s(ACTIONS) + (s(ACTIONS) - s(30)) / 2;
        let action = |left: i32, width: i32| RECT {
            left,
            top: actions_top,
            right: left + width,
            bottom: actions_top + s(30),
        };
        let (text, primary, secondary) = match &self.phase {
            Phase::Failed(_) => {
                let retry = action(body.left, button(RETRY_LABEL));
                let settings = action(retry.right + s(8), button(SETTINGS_LABEL));
                (RECT { bottom: body.bottom - s(ACTIONS), ..body }, Some(retry), Some(settings))
            }
            Phase::Unconfigured => {
                let width = button(SETUP_LABEL);
                let center = (body.top + body.bottom) / 2;
                let setup = RECT {
                    left: body.left,
                    top: center + s(28),
                    right: body.left + width,
                    bottom: center + s(28 + 30),
                };
                (body, Some(setup), None)
            }
            _ => (body, None, None),
        };
        Layout {
            scale,
            panel,
            toolbar,
            language,
            copy,
            settings,
            text,
            primary,
            secondary,
        }
    }

    pub fn hit(&self, layout: &Layout, point: POINT) -> Option<Target> {
        layout
            .targets()
            .find(|(_, rect)| contains(*rect, point))
            .map(|(target, _)| target)
    }

    /// The text that scrolls, if any.
    fn scrolling_text(&self) -> Option<&str> {
        match &self.phase {
            Phase::Done { text, .. } => Some(text),
            Phase::Failed(error) => Some(error),
            _ => None,
        }
    }

    unsafe fn max_scroll(&self, layout: &Layout, dc: HDC, fonts: &Fonts) -> i32 {
        let Some(text) = self.scrolling_text() else { return 0 };
        let area = layout.text;
        let full = ui::wrapped_height(dc, text, fonts.body, area.right - area.left, WRAP);
        (full - (area.bottom - area.top)).max(0)
    }

    /// Scrolls by `delta` pixels. True if it moved.
    pub unsafe fn scroll_by(&mut self, layout: &Layout, dc: HDC, fonts: &Fonts, delta: i32) -> bool {
        let next = (self.scroll + delta).clamp(0, self.max_scroll(layout, dc, fonts));
        std::mem::replace(&mut self.scroll, next) != next
    }

    pub unsafe fn paint(&self, dc: HDC, layout: &Layout, fonts: &Fonts, now_ms: u32) {
        let palette = theme::palette();
        let s = |value: i32| layout.s(value);
        ui::fill(dc, layout.panel, palette.card);
        ui::fill(
            dc,
            RECT { top: layout.toolbar.bottom - 1, ..layout.toolbar },
            palette.divider,
        );
        let face = |target: Target| match (self.pressed == Some(target), self.hovered == Some(target)) {
            (true, _) => palette.pressed,
            (_, true) => palette.hover,
            _ => palette.card,
        };
        let radius = 7.0 * layout.scale;

        // The language picker: icon, label, chevron.
        let language = layout.language;
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(language), radius, argb(face(Target::Language), 255));
            graphics.stroke_round(ui::inset(ui::to_f(language), 0.5), radius, argb(palette.key_border, 255), 1.0);
        }
        let icon = |left: i32, size: i32| RECT {
            left,
            top: (language.top + language.bottom - size) / 2,
            right: left + size,
            bottom: (language.top + language.bottom + size) / 2,
        };
        icons::paint(dc, icon(language.left + s(8), s(16)), Icon::Translate, palette.accent);
        ui::text(
            dc,
            RECT { left: language.left + s(8 + 16 + 6), right: language.right - s(14 + 6), ..language },
            &self.language_label(),
            fonts.small,
            palette.text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        icons::paint(dc, icon(language.right - s(6 + 14), s(14)), Icon::ChevronDown, palette.secondary);

        if let Some(copy) = layout.copy {
            ui::button(dc, copy, COPY_LABEL, fonts.small, palette, false, face(Target::Copy), radius);
        }
        let settings = layout.settings;
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(settings), radius, argb(face(Target::Settings), 255));
        }
        icons::paint(dc, ui::inflate(settings, -s(6)), Icon::Settings, palette.secondary);

        let body = layout.text;
        match &self.phase {
            Phase::Recognizing => self.paint_waiting(dc, layout, fonts, "正在识别文字…", now_ms),
            Phase::Translating { provider, .. } => {
                let caption = match provider {
                    Some(provider) => format!("正在用 {} 翻译…", provider.name()),
                    None => "正在翻译…".to_string(),
                };
                self.paint_waiting(dc, layout, fonts, &caption, now_ms)
            }
            Phase::Done { text, .. } => self.paint_scrolling(dc, layout, fonts, text, palette.text),
            Phase::NoText => ui::text(
                dc,
                body,
                "没有识别到文字，无法翻译",
                fonts.body,
                palette.secondary,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            ),
            Phase::Unconfigured => {
                let center = (body.top + body.bottom) / 2;
                ui::text(
                    dc,
                    RECT { top: center - s(56), bottom: center - s(30), ..body },
                    "还没有配置翻译服务",
                    fonts.heading,
                    palette.text,
                    DT_LEFT | DT_BOTTOM | DT_SINGLELINE,
                );
                ui::text(
                    dc,
                    RECT { top: center - s(24), bottom: center + s(22), ..body },
                    "支持 OpenAI 兼容接口、DeepL、百度和有道。密钥只保存在本机的 Windows 凭据管理器里。",
                    fonts.small,
                    palette.secondary,
                    DT_LEFT | DT_TOP | WRAP,
                );
            }
            Phase::Failed(error) => self.paint_scrolling(dc, layout, fonts, error, palette.error),
        }
        if let Some(primary) = layout.primary {
            let label = if matches!(self.phase, Phase::Failed(_)) { RETRY_LABEL } else { SETUP_LABEL };
            let fill = if self.pressed == Some(Target::Primary) {
                palette.accent_hover
            } else if self.hovered == Some(Target::Primary) {
                mix(palette.accent, palette.accent_hover, 0.5)
            } else {
                palette.accent
            };
            ui::button(dc, primary, label, fonts.small, palette, true, fill, radius);
        }
        if let Some(secondary) = layout.secondary {
            ui::button(dc, secondary, SETTINGS_LABEL, fonts.small, palette, false, face(Target::Secondary), radius);
        }
    }

    /// Skeleton lines with a moving sheen, and what is being waited for.
    unsafe fn paint_waiting(&self, dc: HDC, layout: &Layout, fonts: &Fonts, caption: &str, now_ms: u32) {
        let palette = theme::palette();
        let s = |value: i32| layout.s(value) as f32;
        let body = ui::to_f(layout.text);
        ui::text(
            dc,
            layout.text,
            caption,
            fonts.small,
            palette.secondary,
            DT_LEFT | DT_TOP | DT_SINGLELINE,
        );
        let Some(graphics) = Graphics::new(dc) else { return };
        let width = body[2] - body[0];
        let phase = (now_ms as f32 % SHIMMER_MS) / SHIMMER_MS;
        // The sheen sweeps from left to right across every bar at once.
        let sheen = body[0] - width * 0.4 + phase * width * 1.8;
        let base = mix(palette.card, palette.text, 0.08);
        let bright = mix(palette.card, palette.text, 0.16);
        let mut top = body[1] + s(28);
        for (index, fraction) in [0.92, 0.78, 0.86, 0.6, 0.82, 0.7].into_iter().enumerate() {
            if top + s(12) > body[3] {
                break;
            }
            let right = body[0] + width * fraction;
            graphics.fill_round([body[0], top, right, top + s(12)], s(6), argb(base, 255));
            let left = (sheen - s(60)).max(body[0]);
            let end = (sheen + s(60)).min(right);
            if end > left {
                graphics.fill_round([left, top, end, top + s(12)], s(6), argb(bright, 255));
            }
            top += if index % 3 == 2 { s(30) } else { s(22) };
        }
    }

    /// Wrapped text clipped to its area, shifted by the scroll, with a thumb.
    unsafe fn paint_scrolling(&self, dc: HDC, layout: &Layout, fonts: &Fonts, text: &str, color: u32) {
        let palette = theme::palette();
        let area = layout.text;
        let full = ui::wrapped_height(dc, text, fonts.body, area.right - area.left, WRAP);
        let max_scroll = (full - (area.bottom - area.top)).max(0);
        let scroll = self.scroll.min(max_scroll);
        let saved = SaveDC(dc);
        IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
        ui::text(
            dc,
            RECT { top: area.top - scroll, bottom: area.top - scroll + full, ..area },
            text,
            fonts.body,
            color,
            DT_LEFT | DT_TOP | WRAP,
        );
        RestoreDC(dc, saved);
        if max_scroll == 0 {
            return;
        }
        let s = |value: f32| value * layout.scale;
        let visible = (area.bottom - area.top) as f32;
        let thumb = (visible * visible / full as f32).max(s(24.0));
        let top = area.top as f32 + (visible - thumb) * scroll as f32 / max_scroll as f32;
        let left = area.right as f32 + s(PAD as f32 / 2.0 - 2.0);
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round([left, top, left + s(4.0), top + thumb], s(2.0), argb(palette.tertiary, 200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Desktop, Surface};

    /// Renders the panel in each state. Run with `<test binary> pin_translate --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            let fonts = Fonts::new(1.0);
            let (width, height) = (360, 260);
            let panel = RECT { left: 0, top: 0, right: width, bottom: height };
            let done = "Recognize text. Snapshot saves to C:\\Users\\eric\\Pictures\nDrag over the text to select it and press Ctrl+C to copy.\nThe translation keeps the original line breaks, and long text scrolls with the mouse wheel inside this panel so nothing is cut off.";
            let states: Vec<(&str, Phase, Option<Target>)> = vec![
                ("recognizing", Phase::Recognizing, None),
                ("translating", Phase::Translating { to: Language::English, provider: Some(Provider::DeepL) }, None),
                ("done", Phase::Done { text: done.into(), to: Language::English }, Some(Target::Copy)),
                ("unconfigured", Phase::Unconfigured, Some(Target::Primary)),
                (
                    "failed",
                    Phase::Failed("翻译失败（OpenAI 兼容）\n认证失败，请检查密钥是否正确（HTTP 401）\n服务器返回：Incorrect API key provided: sk-proj-****. You can find your API key at https://platform.openai.com/account/api-keys.".into()),
                    None,
                ),
            ];
            for (name, phase, hovered) in states {
                let mut pane = TranslatePane::new();
                pane.phase = phase;
                pane.hovered = hovered;
                let surface = Surface::new(std::ptr::null_mut(), width, height).unwrap();
                let layout = pane.layout(panel, 1.0, surface.dc, &fonts);
                pane.paint(surface.dc, &layout, &fonts, 500);
                GdiFlush();
                let mut image = Desktop { bounds: panel, surface }.crop(panel).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("pin-translate-{name}.png"))).unwrap();
            }
        }
    }
}
