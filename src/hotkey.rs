use std::{
    cell::Cell,
    env, fs, io,
    path::{Path, PathBuf},
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shortcut {
    pub modifiers: u32,
    pub key: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Binding {
    Capture,
    Rectangle,
    Text,
    Brush,
    Mosaic,
    Arrow,
    Number,
    Magnify,
    Select,
    Undo,
    Redo,
    ExtractText,
    Translate,
    LongCapture,
    Pin,
    Save,
    Copy,
    /// Global: capture the active window.
    CaptureWindow,
    /// Global: bring back the last capture with its annotations.
    LastCapture,
    /// In the capture: show or hide the mouse pointer in the picture.
    ShowPointer,
    /// Global: pick an area and record it; while recording, stop and save.
    Record,
    /// Global, registered only while recording: pause or resume.
    RecordPause,
    /// Global: record the whole screen the active window (a game) is on; again to stop.
    RecordScreen,
    /// Global: show or hide the key and mouse panel.
    ShowKeys,
    /// Global: show or hide the gamepad panel.
    ShowPad,
}

/// What a global shortcut does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Global {
    Capture,
    Window,
    Last,
    Record,
    RecordPause,
    RecordScreen,
    ShowKeys,
    ShowPad,
}

/// In toolbar order, so the settings list reads like the toolbar.
pub const BINDINGS: [Binding; 25] = [
    Binding::Capture,
    Binding::Rectangle,
    Binding::Text,
    Binding::Brush,
    Binding::Mosaic,
    Binding::Arrow,
    Binding::Number,
    Binding::Magnify,
    Binding::Select,
    Binding::Undo,
    Binding::Redo,
    Binding::ExtractText,
    Binding::Translate,
    Binding::LongCapture,
    Binding::Pin,
    Binding::Save,
    Binding::Copy,
    Binding::CaptureWindow,
    Binding::LastCapture,
    Binding::ShowPointer,
    Binding::Record,
    Binding::RecordPause,
    Binding::RecordScreen,
    Binding::ShowKeys,
    Binding::ShowPad,
];
const NAMES: [&str; BINDINGS.len()] = [
    "capture",
    "rectangle",
    "text",
    "brush",
    "mosaic",
    "arrow",
    "number",
    "magnify",
    "select",
    "undo",
    "redo",
    "extract_text",
    "translate",
    "long_capture",
    "pin",
    "save",
    "copy",
    "capture_window",
    "last_capture",
    "show_pointer",
    "record",
    "record_pause",
    "record_screen",
    "show_keys",
    "show_pad",
];
const LABELS: [&str; BINDINGS.len()] = [
    "开始截图（全局）",
    "矩形",
    "文本",
    "画笔",
    "马赛克",
    "箭头",
    "序号",
    "放大镜",
    "选择",
    "撤回",
    "重做",
    "提取文本",
    "翻译",
    "长截图",
    "贴图",
    "保存",
    "复制",
    "截取窗口",
    "上次截图",
    "显示 / 隐藏指针",
    "开始 / 停止录像",
    "暂停 / 继续",
    "全屏 / 游戏录像",
    "键鼠显示",
    "手柄显示",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bindings([Shortcut; BINDINGS.len()]);

pub const DEFAULT_OPACITY_STEP: u8 = 5;
const OPACITY_STEP_KEY: &str = "pin_opacity_step";
const THEME_KEY: &str = "theme";
const SECONDARY_KEY: &str = "capture_secondary";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preferences {
    pub bindings: Bindings,
    pub opacity_step: u8,
    pub theme: Theme,
    /// Optional second global capture shortcut, active alongside the main one.
    pub capture_secondary: Option<Shortcut>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            bindings: DEFAULT,
            opacity_step: DEFAULT_OPACITY_STEP,
            theme: Theme::default(),
            capture_secondary: None,
        }
    }
}

pub fn parse_opacity_step(value: &str) -> Result<u8, String> {
    value
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|step| (1..=100).contains(step))
        .ok_or_else(|| "贴图不透明度步长必须是 1 到 100 的整数。".into())
}

const fn ctrl(key: u8) -> Shortcut {
    Shortcut {
        modifiers: MOD_CONTROL,
        key: key as u32,
    }
}

/// Drawing tools take Ctrl+1… in toolbar order; from 撤回 on, each action
/// has its own mnemonic key.
pub const DEFAULT: Bindings = Bindings([
    Shortcut {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: b'A' as u32,
    },
    ctrl(b'1'),
    ctrl(b'2'),
    ctrl(b'3'),
    ctrl(b'4'),
    ctrl(b'5'),
    ctrl(b'6'),
    ctrl(b'7'),
    ctrl(b'8'),
    ctrl(b'Z'),
    ctrl(b'Y'),
    ctrl(b'E'),
    ctrl(b'F'),
    ctrl(b'L'),
    ctrl(b'T'),
    ctrl(b'S'),
    ctrl(b'C'),
    Shortcut {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: b'W' as u32,
    },
    Shortcut {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: b'R' as u32,
    },
    // A single key: it only works inside the capture, never system-wide.
    Shortcut { modifiers: 0, key: b'P' as u32 },
    ctrl_alt(b'Z'),
    ctrl_alt(b'P'),
    // Alt+F8, next to the Alt+F9 game recorders use.
    Shortcut {
        modifiers: MOD_ALT,
        key: VK_F8 as u32,
    },
    ctrl_alt(b'K'),
    ctrl_alt(b'G'),
]);

const fn ctrl_alt(key: u8) -> Shortcut {
    Shortcut {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: key as u32,
    }
}

/// Defaults before the layout above (files without `layout=2`). A binding
/// still on its old default moves to the new one.
const OLD_DEFAULTS: [(&str, u8); 15] = [
    ("rectangle", b'1'),
    ("text", b'2'),
    ("brush", b'3'),
    ("mosaic", b'4'),
    ("undo", b'5'),
    ("redo", b'6'),
    ("long_capture", b'7'),
    ("copy", b'C'),
    ("save", b'S'),
    ("pin", b'T'),
    ("extract_text", b'8'),
    ("translate", b'9'),
    ("arrow", b'R'),
    ("number", b'N'),
    ("magnify", b'M'),
];
const LAYOUT_KEY: &str = "layout";
const LAYOUT: &str = "2";

thread_local! {
    static CURRENT: Cell<Bindings> = const { Cell::new(DEFAULT) };
}

pub fn current() -> Bindings {
    CURRENT.get()
}

pub fn activate(bindings: Bindings) {
    CURRENT.set(bindings);
}

impl Binding {
    pub fn label(self) -> &'static str {
        LABELS[self as usize]
    }

    /// May be a single letter or digit with no modifier. Only for keys used
    /// inside the capture; a global one would take the letter from every program.
    pub fn allows_single_key(self) -> bool {
        self == Self::ShowPointer
    }

    /// Registered with Windows, so it works in any program.
    pub fn global(self) -> Option<Global> {
        match self {
            Self::Capture => Some(Global::Capture),
            Self::CaptureWindow => Some(Global::Window),
            Self::LastCapture => Some(Global::Last),
            Self::Record => Some(Global::Record),
            Self::RecordPause => Some(Global::RecordPause),
            Self::RecordScreen => Some(Global::RecordScreen),
            Self::ShowKeys => Some(Global::ShowKeys),
            Self::ShowPad => Some(Global::ShowPad),
            _ => None,
        }
    }
}

impl Bindings {
    pub fn get(self, binding: Binding) -> Shortcut {
        self.0[binding as usize]
    }

    pub fn set(&mut self, binding: Binding, shortcut: Shortcut) {
        self.0[binding as usize] = shortcut;
    }

    pub fn resolve(self, shortcut: Shortcut) -> Option<Binding> {
        if let Some(binding) = BINDINGS
            .into_iter()
            .find(|&binding| self.get(binding) == shortcut)
        {
            return binding.global().is_none().then_some(binding);
        }
        match shortcut {
            Shortcut { modifiers: 0, key } if key == u32::from(VK_RETURN) => Some(Binding::Copy),
            Shortcut { modifiers, key }
                if self.get(Binding::Redo) == DEFAULT.get(Binding::Redo)
                    && modifiers == MOD_CONTROL | MOD_SHIFT
                    && key == u32::from(b'Z') =>
            {
                Some(Binding::Redo)
            }
            _ => None,
        }
    }

    pub fn validate(self) -> Result<Self, String> {
        for (index, binding) in BINDINGS.into_iter().enumerate() {
            let shortcut = self.get(binding);
            if let Some(other) = BINDINGS[..index]
                .iter()
                .find(|&&other| self.get(other) == shortcut)
            {
                return Err(format!(
                    "「{}」和「{}」都绑定了 {}。",
                    other.label(),
                    binding.label(),
                    shortcut.label(),
                ));
            }
        }
        Ok(self)
    }
}

impl Shortcut {
    pub fn from_key(modifiers: u32, key: u32) -> Option<Self> {
        let function_key =
            (u32::from(VK_F1)..=u32::from(VK_F24)).contains(&key) && key != u32::from(VK_F12);
        let alphanumeric =
            u8::try_from(key).is_ok_and(|key| key.is_ascii_uppercase() || key.is_ascii_digit());
        (function_key || (modifiers != 0 && alphanumeric)).then_some(Self { modifiers, key })
    }

    /// Like `parse`, also taking a lone letter or digit where `binding` allows it.
    pub fn parse_for(binding: Binding, value: &str) -> Option<Self> {
        Self::parse(value).or_else(|| Self::single(binding, value.trim()))
    }

    /// A lone letter or digit for a binding that allows one.
    pub fn single(binding: Binding, value: &str) -> Option<Self> {
        let key = match value.to_ascii_uppercase().as_bytes() {
            [key @ (b'A'..=b'Z' | b'0'..=b'9')] => u32::from(*key),
            _ => return None,
        };
        binding.allows_single_key().then_some(Self { modifiers: 0, key })
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.to_ascii_uppercase();
        let mut parts = normalized.split('+').map(str::trim);
        let key = parts.next_back()?;
        let mut modifiers = 0;
        for part in parts {
            let modifier = match part {
                "CTRL" => MOD_CONTROL,
                "ALT" => MOD_ALT,
                "SHIFT" => MOD_SHIFT,
                "WIN" => MOD_WIN,
                _ => return None,
            };
            if modifiers & modifier != 0 {
                return None;
            }
            modifiers |= modifier;
        }
        let key = match key.as_bytes() {
            [key @ (b'A'..=b'Z' | b'0'..=b'9')] => u32::from(*key),
            _ => {
                let number = key.strip_prefix('F')?.parse::<u32>().ok()?;
                if !(1..=24).contains(&number) {
                    return None;
                }
                u32::from(VK_F1) + number - 1
            }
        };
        Self::from_key(modifiers, key)
    }

    pub fn label(self) -> String {
        let mut label = String::new();
        for (mask, text) in [
            (MOD_CONTROL, "Ctrl+"),
            (MOD_ALT, "Alt+"),
            (MOD_SHIFT, "Shift+"),
            (MOD_WIN, "Win+"),
        ] {
            if self.modifiers & mask != 0 {
                label.push_str(text);
            }
        }
        if self.key >= u32::from(VK_F1) {
            label.push_str(&format!("F{}", self.key - u32::from(VK_F1) + 1));
        } else {
            label.push(self.key as u8 as char);
        }
        label
    }
}

pub unsafe fn modifiers() -> u32 {
    let pressed = |key| GetKeyState(i32::from(key)) < 0;
    u32::from(pressed(VK_CONTROL)) * MOD_CONTROL
        | u32::from(pressed(VK_MENU)) * MOD_ALT
        | u32::from(pressed(VK_SHIFT)) * MOD_SHIFT
        | u32::from(pressed(VK_LWIN) || pressed(VK_RWIN)) * MOD_WIN
}

pub fn path() -> Result<PathBuf, String> {
    let app_data = env::var_os("LOCALAPPDATA").ok_or("找不到 LOCALAPPDATA。")?;
    Ok(PathBuf::from(app_data)
        .join("Snapshot")
        .join("shortcuts.txt"))
}

/// No settings file and no legacy hotkey file means Snapshot has never run here.
pub fn first_run() -> bool {
    path().is_ok_and(|path| !path.exists() && !path.with_file_name("hotkey.txt").exists())
}

pub fn load() -> Result<Preferences, String> {
    load_from(&path()?)
}

fn load_from(path: &Path) -> Result<Preferences, String> {
    let mut bindings = DEFAULT;
    let mut opacity_step = DEFAULT_OPACITY_STEP;
    let mut theme = Theme::default();
    let mut capture_secondary = None;
    let mut layout = None;
    let mut stored: Vec<(usize, Shortcut)> = Vec::new();
    match fs::read_to_string(path) {
        Ok(contents) => {
            for line in contents
                .trim_start_matches('\u{feff}')
                .lines()
                .filter(|line| !line.trim().is_empty())
            {
                let (name, value) = line
                    .split_once('=')
                    .ok_or("快捷键配置格式应为 名称=组合键。")?;
                let name = name.trim();
                if name == OPACITY_STEP_KEY {
                    opacity_step = parse_opacity_step(value)?;
                } else if name == SECONDARY_KEY {
                    let value = value.trim();
                    capture_secondary = if value.is_empty() {
                        None
                    } else {
                        Some(
                            Shortcut::parse(value)
                                .ok_or_else(|| format!("快捷键格式有误：{line}"))?,
                        )
                    };
                } else if name == LAYOUT_KEY {
                    layout = Some(value.trim().to_string());
                } else if name == THEME_KEY {
                    // An unknown theme falls back to the default instead of failing startup.
                    theme = Theme::parse(value).unwrap_or_default();
                } else if let Some(index) = NAMES.iter().position(|&key| key == name) {
                    let shortcut = Shortcut::parse_for(BINDINGS[index], value).ok_or_else(|| format!("快捷键格式有误：{line}"))?;
                    bindings.set(BINDINGS[index], shortcut);
                    stored.push((index, shortcut));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let legacy = path.with_file_name("hotkey.txt");
            match fs::read_to_string(&legacy) {
                Ok(value) => {
                    let capture = Shortcut::parse(value.trim_start_matches('\u{feff}'))
                        .ok_or("原截图快捷键格式有误，请检查 hotkey.txt。")?;
                    bindings.set(Binding::Capture, capture);
                    // Legacy files could use an in-app default as the global trigger.
                    if let Some(binding) = BINDINGS[1..]
                        .iter()
                        .find(|&&binding| bindings.get(binding) == capture)
                    {
                        if let Some(replacement) = (b'0'..=b'9')
                            .map(ctrl)
                            .find(|&shortcut| !bindings.0.contains(&shortcut))
                        {
                            bindings.set(*binding, replacement);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(error) => return Err(format!("无法读取 {}。\n{error}", path.display())),
    }
    if !stored.is_empty() && layout.as_deref() != Some(LAYOUT) {
        let mut migrated = bindings;
        for (index, shortcut) in &stored {
            let old = OLD_DEFAULTS.iter().find(|(name, _)| *name == NAMES[*index]);
            if old.is_some_and(|&(_, key)| ctrl(key) == *shortcut) {
                migrated.set(BINDINGS[*index], DEFAULT.get(BINDINGS[*index]));
            }
        }
        // Keys the user picked that now collide keep the old layout instead.
        if migrated.validate().is_ok() {
            bindings = migrated;
        }
    }
    // A binding added in a newer version takes its default unless the user
    // already uses that key; then it gets the first free one.
    for binding in BINDINGS {
        let index = binding as usize;
        if stored.iter().any(|(stored, _)| *stored == index) {
            continue;
        }
        let taken = |shortcut: Shortcut| BINDINGS.iter().any(|&other| other != binding && bindings.get(other) == shortcut);
        if taken(bindings.get(binding)) {
            let spare = (b'0'..=b'9')
                .map(ctrl)
                .chain((b'0'..=b'9').map(|key| Shortcut { modifiers: MOD_CONTROL | MOD_SHIFT, key: u32::from(key) }))
                .find(|&shortcut| !taken(shortcut));
            if let Some(spare) = spare {
                bindings.set(binding, spare);
            }
        }
    }
    let preferences = Preferences {
        bindings: bindings.validate()?,
        opacity_step,
        theme,
        capture_secondary,
    };
    preferences.validate()?;
    Ok(preferences)
}

impl Preferences {
    /// The global shortcuts: the main capture key, the optional second one,
    /// then every other global binding. The pause key is only taken while
    /// `recording`, so other programs keep it the rest of the time.
    pub fn globals(&self, recording: bool) -> Vec<(Shortcut, Global)> {
        std::iter::once((self.bindings.get(Binding::Capture), Global::Capture))
            .chain(self.capture_secondary.map(|shortcut| (shortcut, Global::Capture)))
            .chain(
                BINDINGS[1..]
                    .iter()
                    .filter(|&&binding| recording || binding != Binding::RecordPause)
                    .filter_map(|&binding| Some((self.bindings.get(binding), binding.global()?))),
            )
            .collect()
    }

    /// The second capture shortcut must not collide with any other binding.
    pub fn validate(&self) -> Result<(), String> {
        let Some(secondary) = self.capture_secondary else {
            return Ok(());
        };
        if let Some(binding) = BINDINGS
            .into_iter()
            .find(|&binding| self.bindings.get(binding) == secondary)
        {
            return Err(format!(
                "副截图键和「{}」都绑定了 {}。",
                binding.label(),
                secondary.label()
            ));
        }
        Ok(())
    }
}

pub fn save(preferences: Preferences) -> Result<(), String> {
    save_to(&path()?, preferences)
}

fn save_to(path: &Path, preferences: Preferences) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    let mut contents: String = BINDINGS
        .into_iter()
        .map(|binding| {
            format!(
                "{}={}\r\n",
                NAMES[binding as usize],
                preferences.bindings.get(binding).label()
            )
        })
        .collect();
    contents.push_str(&format!(
        "{SECONDARY_KEY}={}\r\n{OPACITY_STEP_KEY}={}\r\n{THEME_KEY}={}\r\n{LAYOUT_KEY}={LAYOUT}\r\n",
        preferences
            .capture_secondary
            .map(Shortcut::label)
            .unwrap_or_default(),
        preferences.opacity_step,
        preferences.theme.key()
    ));
    fs::create_dir_all(path.parent().expect("settings directory"))
        .map_err(|error| error.to_string())?;
    fs::write(&temporary, contents).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(temporary);
        return Err(format!("无法保存设置。\n{error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bindings_and_rejects_reserved_or_ambiguous_values() {
        assert_eq!(
            Shortcut::parse("Ctrl+Alt+A"),
            Some(DEFAULT.get(Binding::Capture))
        );
        assert_eq!(
            Shortcut::parse("ctrl + Shift + f10").unwrap().label(),
            "Ctrl+Shift+F10"
        );
        assert_eq!(Shortcut::parse("F1").unwrap().label(), "F1");
        assert_eq!(DEFAULT.get(Binding::ShowPointer).label(), "P", "the pointer key is a single key");
        assert_eq!(Shortcut::parse_for(Binding::ShowPointer, "m"), Some(Shortcut { modifiers: 0, key: u32::from(b'M') }));
        assert_eq!(Shortcut::parse_for(Binding::Capture, "M"), None, "global keys need a modifier");
        for value in [
            "9",
            "Ctrl+Ctrl+9",
            "Ctrl+",
            "Ctrl+9+X",
            "Ctrl+F12",
            "Ctrl+F25",
        ] {
            assert_eq!(Shortcut::parse(value), None, "{value}");
        }
    }

    #[test]
    fn old_default_layouts_move_to_the_toolbar_order() {
        let folder = env::temp_dir().join(format!("snapshot-layout-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("shortcuts.txt");
        // An old file: defaults everywhere except a custom brush key.
        fs::write(&file, "capture=Ctrl+Alt+A\r\nrectangle=Ctrl+1\r\nbrush=Ctrl+Shift+B\r\nundo=Ctrl+5\r\nredo=Ctrl+6\r\nlong_capture=Ctrl+7\r\nextract_text=Ctrl+8\r\ntranslate=Ctrl+9\r\narrow=Ctrl+R\r\nnumber=Ctrl+N\r\n").unwrap();
        let bindings = load_from(&file).unwrap().bindings;
        assert_eq!(bindings.get(Binding::Arrow), ctrl(b'5'));
        assert_eq!(bindings.get(Binding::Number), ctrl(b'6'));
        assert_eq!(bindings.get(Binding::Undo), ctrl(b'Z'));
        assert_eq!(bindings.get(Binding::ExtractText), ctrl(b'E'));
        assert_eq!(bindings.get(Binding::Brush), Shortcut::parse("Ctrl+Shift+B").unwrap(), "custom keys stay");
        // Once saved with the new layout, Ctrl+5 for undo is a real choice.
        let mut preferences = Preferences::default();
        preferences.bindings.set(Binding::Arrow, ctrl(b'R'));
        preferences.bindings.set(Binding::Undo, ctrl(b'5'));
        save_to(&file, preferences).unwrap();
        assert_eq!(load_from(&file).unwrap().bindings.get(Binding::Undo), ctrl(b'5'));
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_new_binding_never_collides_with_a_saved_key() {
        let folder = env::temp_dir().join(format!("snapshot-new-binding-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("shortcuts.txt");
        // Saved before 选择 existed, with Ctrl+8 given to copy.
        fs::write(&file, "copy=Ctrl+8\r\nlayout=2\r\n").unwrap();
        let bindings = load_from(&file).unwrap().bindings;
        assert_eq!(bindings.get(Binding::Copy), ctrl(b'8'));
        assert_ne!(bindings.get(Binding::Select), ctrl(b'8'));
        assert!(bindings.validate().is_ok());
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn tools_count_up_from_ctrl_1_in_toolbar_order() {
        let tools = [Binding::Rectangle, Binding::Text, Binding::Brush, Binding::Mosaic, Binding::Arrow, Binding::Number, Binding::Magnify, Binding::Select];
        for (index, tool) in tools.into_iter().enumerate() {
            assert_eq!(DEFAULT.get(tool), ctrl(b'1' + index as u8));
            assert_eq!(BINDINGS[index + 1], tool);
        }
        assert_eq!(DEFAULT.get(Binding::Undo).label(), "Ctrl+Z");
        assert!(DEFAULT.validate().is_ok());
    }

    #[test]
    fn explicit_bindings_take_priority_and_duplicates_are_rejected() {
        let mut bindings = DEFAULT;
        bindings.set(Binding::Copy, ctrl(b'J'));
        assert_eq!(bindings.resolve(ctrl(b'J')), Some(Binding::Copy));
        assert!(bindings.validate().is_ok());
        bindings.set(Binding::Save, ctrl(b'J'));
        assert!(bindings.validate().is_err());
    }

    #[test]
    fn reads_legacy_capture_and_round_trips_settings_on_repeated_saves() {
        let folder =
            env::temp_dir().join(format!("snapshot-shortcuts-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let legacy = folder.join("hotkey.txt");
        let file = folder.join("shortcuts.txt");
        fs::write(&legacy, "Ctrl+F8\r\n").unwrap();
        let mut preferences = load_from(&file).unwrap();
        assert_eq!(preferences.opacity_step, 5);
        assert_eq!(
            preferences.bindings.get(Binding::Capture),
            Shortcut::parse("Ctrl+F8").unwrap()
        );
        preferences
            .bindings
            .set(Binding::Copy, Shortcut::parse("Ctrl+Shift+C").unwrap());
        preferences.opacity_step = 10;
        preferences.theme = Theme::Midnight;
        preferences.capture_secondary = Shortcut::parse("F1");
        assert_eq!(preferences.globals(false).len(), 8);
        assert_eq!(preferences.globals(true).len(), 9, "the pause key only while recording");
        save_to(&file, preferences).unwrap();
        assert_eq!(load_from(&file).unwrap(), preferences);
        save_to(&file, Preferences::default()).unwrap();
        assert_eq!(load_from(&file).unwrap(), Preferences::default());
        fs::remove_file(legacy).unwrap();
        fs::remove_file(file).unwrap();
        fs::remove_dir(folder).unwrap();
    }
}
