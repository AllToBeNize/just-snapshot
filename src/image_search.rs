//! Reverse image search: the capture is uploaded to an engine and its result
//! page opens in the default browser. Google Lens and Yandex come built in;
//! the user can change them and add any service that takes a multipart upload
//! and answers with a redirect or JSON. Engines live in `image_search.txt`.
//!
//! Redirect engines are uploaded to by the browser itself: Lens ties its
//! result to the session cookie it sets on the upload, so a page uploaded by
//! the app shows up empty in the browser. A local page with the picture inside
//! submits the form and the browser follows the redirect with its own cookies.

use std::{
    fs, io,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, POINT},
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::*},
    },
};

use crate::{capture::Image, http, wide};

/// The engines that ship with the app; their fields can be reset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preset {
    Google,
    Yandex,
}

pub const PRESETS: [Preset; 2] = [Preset::Google, Preset::Yandex];

impl Preset {
    fn key(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Yandex => "yandex",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        PRESETS.into_iter().find(|preset| preset.key() == value)
    }

    /// The engine as it ships.
    pub fn engine(self) -> Engine {
        let (name, url, field, json, link) = match self {
            // Lens answers the upload with a 303 to its result page, tied to
            // the uploading session: the browser uploads.
            Self::Google => (
                "Google",
                "https://lens.google.com/v3/upload?ep=ccm&re=dcsp&s=4&st={timestamp}",
                "encoded_image",
                "",
                "",
            ),
            // yandex.ru answers with JSON naming the uploaded picture; yandex.com
            // answers with a page instead.
            Self::Yandex => (
                "Yandex",
                "https://yandex.ru/images/search?rpt=imageview&format=json&request=%7B%22blocks%22%3A%5B%7B%22block%22%3A%22b-page_type_search-by-image__link%22%7D%5D%7D",
                "upfile",
                "blocks.0.params.cbirId",
                "https://yandex.ru/images/search?rpt=imageview&cbir_id={value}",
            ),
        };
        Engine {
            preset: Some(self),
            name: name.into(),
            url: url.into(),
            field: field.into(),
            json: json.into(),
            link: link.into(),
        }
    }
}

/// One search service.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Engine {
    pub preset: Option<Preset>,
    pub name: String,
    /// Where the picture is POSTed; `{timestamp}` becomes the time in ms.
    pub url: String,
    /// The multipart field the picture goes in.
    pub field: String,
    /// Dot path to the result in the JSON answer (`blocks.0.params.cbirId`);
    /// empty lets the browser upload and follow the redirect itself.
    pub json: String,
    /// The result page with `{value}` for what was found; empty uses the
    /// value itself as the address.
    pub link: String,
}

/// The editable fields, in the order the settings window shows them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Name,
    Url,
    FileField,
    Json,
    Link,
}

pub const FIELDS: [Field; 5] = [Field::Name, Field::Url, Field::FileField, Field::Json, Field::Link];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Url => "上传地址",
            Self::FileField => "文件字段",
            Self::Json => "结果字段",
            Self::Link => "结果链接",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Name => "例如 Bing",
            Self::Url => "https://…",
            Self::FileField => "例如 file",
            Self::Json => "留空：浏览器上传并跟随跳转",
            Self::Link => "留空：结果本身就是网址",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Url => "url",
            Self::FileField => "field",
            Self::Json => "json",
            Self::Link => "link",
        }
    }
}

impl Engine {
    pub fn get(&self, field: Field) -> &str {
        match field {
            Field::Name => &self.name,
            Field::Url => &self.url,
            Field::FileField => &self.field,
            Field::Json => &self.json,
            Field::Link => &self.link,
        }
    }

    pub fn set(&mut self, field: Field, value: &str) {
        let value = value.trim().to_string();
        match field {
            Field::Name => self.name = value,
            Field::Url => self.url = value,
            Field::FileField => self.field = value,
            Field::Json => self.json = value,
            Field::Link => self.link = value,
        }
    }

    /// What is wrong with the fields, if anything.
    pub fn problem(&self) -> Option<String> {
        let web = |value: &str| value.starts_with("https://") || value.starts_with("http://");
        if self.name.is_empty() {
            Some("请填写「名称」。".into())
        } else if !web(&self.url) {
            Some(format!("「{}」的上传地址要以 https:// 或 http:// 开头。", self.name))
        } else if self.field.is_empty() {
            Some(format!("请填写「{}」的文件字段。", self.name))
        } else if !self.link.is_empty() && !web(&self.link) {
            Some(format!("「{}」的结果链接要以 https:// 或 http:// 开头。", self.name))
        } else {
            None
        }
    }

    /// What to open for the PNG: the result page, or for a redirect engine
    /// the local page that has the browser upload it.
    pub fn search(&self, png: &[u8]) -> Result<String, String> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let url = self.url.replace("{timestamp}", &timestamp.to_string());
        if self.json.is_empty() {
            return self.browser_upload(&url, png, timestamp);
        }
        let upload = http::upload(&url, &self.field, "capture.png", "image/png", png)?;
        self.result(&upload)
    }

    /// Writes the page that submits the PNG to `url` from the browser.
    /// Pages older than ten minutes go first: they hold old captures.
    fn browser_upload(&self, url: &str, png: &[u8], timestamp: u128) -> Result<String, String> {
        let folder = std::env::temp_dir().join("just-snapshot-search");
        let failed = |error: io::Error| format!("无法生成上传页面 {}。\n{error}", folder.display());
        fs::create_dir_all(&folder).map_err(failed)?;
        if let Ok(entries) = fs::read_dir(&folder) {
            for entry in entries.flatten() {
                let old = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age.as_secs() > 600));
                if old {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        let path = folder.join(format!("search-{timestamp}-{:x}.html", hash(&self.name)));
        fs::write(&path, upload_page(&self.name, url, &self.field, png)).map_err(failed)?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// The result page from the JSON answer to an upload.
    fn result(&self, upload: &http::Response) -> Result<String, String> {
        if !(200..300).contains(&upload.status) {
            return Err(unexpected("服务器拒绝了上传", upload));
        }
        let answer: serde_json::Value =
            serde_json::from_str(&upload.body).map_err(|_| unexpected("服务器返回的不是 JSON", upload))?;
        let value = match lookup(&answer, &self.json) {
            Some(value) if !value.is_empty() => value,
            _ => return Err(unexpected(&format!("返回的 JSON 里没有「{}」", self.json), upload)),
        };
        let page = if self.link.is_empty() { value } else { self.link.replace("{value}", &http::encode(&value)) };
        if page.starts_with("https://") || page.starts_with("http://") {
            Ok(page)
        } else {
            Err(format!("拿到的结果不是网址：{}", clip(&page)))
        }
    }
}

fn unexpected(reason: &str, upload: &http::Response) -> String {
    let body = upload.body.trim();
    if body.is_empty() {
        format!("{reason}（HTTP {}）", upload.status)
    } else {
        format!("{reason}（HTTP {}）\n服务器返回：{}", upload.status, clip(body))
    }
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 160;
    let mut clipped: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        clipped.push('…');
    }
    clipped
}

/// Keeps two engines searched in the same millisecond apart.
fn hash(text: &str) -> u32 {
    text.bytes().fold(2166136261u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(16777619))
}

/// A page that puts the PNG into a file field and submits it to `url`, the
/// way the engine's own upload button would.
fn upload_page(name: &str, url: &str, field: &str, png: &[u8]) -> String {
    let attribute = |text: &str| {
        text.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
    };
    let name = attribute(name);
    format!(
        r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>正在上传到 {name}…</title></head>
<body style="margin:0;height:100vh;display:grid;place-items:center;background:#1e1f22;color:#c9ccd1;font:15px 'Microsoft YaHei UI',sans-serif">
<p>just-snapshot 正在把截图上传到 {name}…</p>
<form id="upload" method="post" enctype="multipart/form-data" action="{action}"><input id="file" type="file" name="{field}" hidden></form>
<script>
const data = atob("{data}");
const bytes = new Uint8Array(data.length);
for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i);
const files = new DataTransfer();
files.items.add(new File([bytes], "capture.png", {{ type: "image/png" }}));
document.getElementById("file").files = files.files;
document.getElementById("upload").submit();
</script>
</body></html>
"#,
        action = attribute(url),
        field = attribute(field),
        data = base64(png),
    )
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, &byte)| value | u32::from(byte) << (16 - index * 8));
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(ALPHABET[(value >> (18 - index * 6)) as usize & 63] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// Walks `path` (`a.0.b`) through objects and arrays to a string or number.
fn lookup(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut current = value;
    for step in path.split('.') {
        current = match current {
            serde_json::Value::Array(items) => items.get(step.parse::<usize>().ok()?)?,
            serde_json::Value::Object(map) => map.get(step)?,
            _ => return None,
        };
    }
    match current {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub fn path() -> Result<PathBuf, String> {
    Ok(crate::hotkey::path()?.with_file_name("image_search.txt"))
}

/// The configured engines; the built-in ones until the user saves.
pub fn load() -> Result<Vec<Engine>, String> {
    load_from(&path()?)
}

fn load_from(path: &Path) -> Result<Vec<Engine>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PRESETS.into_iter().map(Preset::engine).collect())
        }
        Err(error) => return Err(format!("无法读取识图设置 {}。\n{error}", path.display())),
    };
    let mut engines: Vec<Engine> = Vec::new();
    for line in contents.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line == "[engine]" {
            engines.push(Engine::default());
            continue;
        }
        let (Some(engine), Some((name, value))) = (engines.last_mut(), line.split_once('=')) else { continue };
        let (name, value) = (name.trim(), value.trim());
        if name == "preset" {
            engine.preset = Preset::parse(value);
        } else if let Some(field) = FIELDS.into_iter().find(|field| field.key() == name) {
            engine.set(field, value);
        }
    }
    engines.retain(|engine| !engine.name.is_empty());
    Ok(engines)
}

pub fn save(engines: &[Engine]) -> Result<(), String> {
    save_to(&path()?, engines)
}

fn save_to(path: &Path, engines: &[Engine]) -> Result<(), String> {
    let mut contents = String::from(
        "# just-snapshot 识图接口。{timestamp} 换成当前毫秒数；json 为空时由浏览器上传并跟随跳转，否则程序上传、按路径从返回的 JSON 取值，代入 link 里的 {value}。\r\n",
    );
    for engine in engines {
        contents.push_str("\r\n[engine]\r\n");
        if let Some(preset) = engine.preset {
            contents.push_str(&format!("preset={}\r\n", preset.key()));
        }
        for field in FIELDS {
            contents.push_str(&format!("{}={}\r\n", field.key(), engine.get(field)));
        }
    }
    let failed = |error: io::Error| format!("无法保存识图设置 {}。\n{error}", path.display());
    fs::create_dir_all(path.parent().expect("settings directory")).map_err(failed)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents).map_err(failed)?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        failed(error)
    })
}

/// What was chosen in an engine menu.
pub enum Pick {
    /// Search with these, each opening its own page.
    Engines(Vec<Engine>),
    /// Open the engine settings.
    Settings,
}

const MENU_ALL: usize = 0x80;
const MENU_SETTINGS: usize = 0x81;
/// How many ids an engine menu takes from its `base`.
pub const MENU_IDS: usize = 0x82;

/// Adds one item per engine, "全部引擎" and the settings to `menu`, with ids from `base`.
pub unsafe fn append_items(menu: HMENU, base: usize, engines: &[Engine]) {
    for (index, engine) in engines.iter().take(MENU_ALL).enumerate() {
        let label = wide(&engine.name.replace('&', "&&"));
        AppendMenuW(menu, MF_STRING, base + index, label.as_ptr());
    }
    if engines.len() > 1 {
        AppendMenuW(menu, MF_STRING, base + MENU_ALL, w!("全部引擎"));
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, base + MENU_SETTINGS, w!("设置识图接口…"));
}

/// What a command id from `append_items` means.
pub fn picked(id: usize, base: usize, mut engines: Vec<Engine>) -> Option<Pick> {
    match id.checked_sub(base)? {
        MENU_SETTINGS => Some(Pick::Settings),
        MENU_ALL => Some(Pick::Engines(engines)),
        index if index < engines.len() => Some(Pick::Engines(vec![engines.swap_remove(index)])),
        _ => None,
    }
}

/// Shows the engine menu at `point` (screen) and returns the choice.
pub unsafe fn choose(window: HWND, point: POINT) -> Result<Option<Pick>, String> {
    let engines = load()?;
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return Ok(None);
    }
    append_items(menu, 1, &engines);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, point.x, point.y, 0, window, null());
    DestroyMenu(menu);
    Ok(picked(chosen as usize, 1, engines))
}

/// The waiting message while `engines` work.
pub fn busy_label(engines: &[Engine]) -> String {
    match engines {
        [engine] => format!("正在用 {} 识图…", engine.name),
        _ => format!("正在用 {} 个引擎识图…", engines.len()),
    }
}

/// Every engine's answer: its name and the result page or why there is none.
pub struct Outcome {
    pub results: Vec<(String, Result<String, String>)>,
}

impl Outcome {
    /// Takes over the outcome posted by `start`.
    pub unsafe fn from_message(lparam: LPARAM) -> Self {
        *Box::from_raw(lparam as *mut Outcome)
    }

    /// Opens every result page. Returns how many opened and, when some
    /// engines failed, a message saying which and why.
    pub unsafe fn open(&self) -> (usize, Option<String>) {
        let mut opened = 0;
        let mut failures = Vec::new();
        for (name, result) in &self.results {
            match result {
                Ok(page) => {
                    let page = wide(page);
                    ShellExecuteW(null_mut(), w!("open"), page.as_ptr(), null(), null(), SW_SHOWNORMAL);
                    opened += 1;
                }
                Err(error) => failures.push((name, error)),
            }
        }
        let message = match failures.as_slice() {
            [] => None,
            [(name, error)] => Some(format!("{name} 识图失败。\n{error}")),
            _ => Some(
                std::iter::once("识图失败：".to_string())
                    .chain(failures.iter().map(|(name, error)| format!("{name}：{error}")))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        (opened, message)
    }
}

/// Searches `image` with every engine at once off the UI thread, then posts
/// `message` to `window` with an owned `Box<Outcome>` in `lparam`. Closing
/// the window meanwhile cancels: nothing opens.
pub fn start(window: HWND, message: u32, engines: Vec<Engine>, image: Image) {
    let target = window as isize;
    std::thread::spawn(move || {
        let outcome = Box::into_raw(Box::new(search_all(&engines, &image)));
        unsafe {
            if PostMessageW(target as HWND, message, 0, outcome as LPARAM) == 0 {
                drop(Box::from_raw(outcome));
            }
        }
    });
}

fn search_all(engines: &[Engine], image: &Image) -> Outcome {
    let mut png = Vec::new();
    if let Err(error) = image.write_png_fast(&mut png) {
        let error = format!("无法生成图片：{error}");
        return Outcome { results: engines.iter().map(|engine| (engine.name.clone(), Err(error.clone()))).collect() };
    }
    let results = std::thread::scope(|scope| {
        let png = &png;
        let workers: Vec<_> = engines.iter().map(|engine| scope.spawn(move || engine.search(png))).collect();
        engines
            .iter()
            .zip(workers)
            .map(|(engine, worker)| {
                let result = worker.join().unwrap_or_else(|_| Err("识图时出错了".into()));
                (engine.name.clone(), result)
            })
            .collect()
    });
    Outcome { results }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::tests::serve;

    fn engine(url: &str, json: &str, link: &str) -> Engine {
        Engine { preset: None, name: "Test".into(), url: url.into(), field: "file".into(), json: json.into(), link: link.into() }
    }

    #[test]
    fn redirect_engines_hand_the_upload_to_the_browser() {
        let mut lens = engine("https://lens.example/upload?st={timestamp}&a=1", "", "");
        lens.name = "Lens <测试>".into();
        let page = lens.search(b"PNG\x00\xff").unwrap();
        assert!(page.ends_with(".html"), "{page}");
        let html = fs::read_to_string(&page).unwrap();
        fs::remove_file(&page).unwrap();
        assert!(html.contains(r#"action="https://lens.example/upload?st="#) && !html.contains("{timestamp}"));
        assert!(html.contains("&amp;a=1") && html.contains("Lens &lt;测试&gt;"), "{html}");
        assert!(html.contains(r#"name="file""#));
        assert!(html.contains(&format!(r#"atob("{}")"#, base64(b"PNG\x00\xff"))));
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn json_engines_upload_from_the_app() {
        let (address, received) = serve(vec![(200, r#"{"url":"https://example.com/r?id=7"}"#.into())]);
        let page = engine(&format!("{address}/up?st={{timestamp}}"), "url", "").search(b"png").unwrap();
        assert_eq!(page, "https://example.com/r?id=7");
        let request = received.recv().unwrap();
        assert!(!request.contains("{timestamp}"), "{request}");
        assert!(request.contains("name=\"file\"; filename=\"capture.png\""));
    }

    #[test]
    fn json_engines_fill_the_link_with_the_encoded_value() {
        let body = r#"{"blocks":[{"params":{"cbirId":"48/tWe u"}}]}"#;
        let (address, _) = serve(vec![(200, body.into())]);
        let page = engine(&address, "blocks.0.params.cbirId", "https://yandex.ru/images/search?cbir_id={value}")
            .search(b"png")
            .unwrap();
        assert_eq!(page, "https://yandex.ru/images/search?cbir_id=48%2FtWe%20u");
    }

    #[test]
    fn failures_say_what_the_server_sent() {
        let (address, _) = serve(vec![
            (200, "<html>captcha</html>".into()),
            (200, r#"{"blocks":[]}"#.into()),
            (403, "denied".into()),
            (200, r#"{"url":"not a page"}"#.into()),
        ]);
        let error = engine(&address, "a", "").search(b"png").unwrap_err();
        assert!(error.contains("不是 JSON") && error.contains("captcha"), "{error}");
        let error = engine(&address, "blocks.0.params.cbirId", "").search(b"png").unwrap_err();
        assert!(error.contains("没有「blocks.0.params.cbirId」"), "{error}");
        let error = engine(&address, "url", "").search(b"png").unwrap_err();
        assert!(error.contains("拒绝了上传") && error.contains("HTTP 403") && error.contains("denied"), "{error}");
        let error = engine(&address, "url", "").search(b"png").unwrap_err();
        assert!(error.contains("不是网址"), "{error}");
    }

    #[test]
    fn engines_round_trip_through_the_file_and_default_to_the_presets() {
        let folder = std::env::temp_dir().join(format!("snapshot-image-search-{}", std::process::id()));
        let path = folder.join("image_search.txt");
        let _ = fs::remove_dir_all(&folder);
        assert_eq!(load_from(&path).unwrap(), vec![Preset::Google.engine(), Preset::Yandex.engine()]);
        let mut google = Preset::Google.engine();
        google.name = "谷歌 = Lens".into();
        let custom = engine("https://example.com/up", "data.url", "");
        save_to(&path, &[google.clone(), custom.clone()]).unwrap();
        assert_eq!(load_from(&path).unwrap(), vec![google, custom]);
        fs::remove_dir_all(&folder).unwrap();
    }

    /// Uploads the app icon to the real built-in engines through the system
    /// proxy: `<test binary> image_search::tests::presets_live --ignored --nocapture`.
    #[test]
    #[ignore]
    fn presets_live() {
        let mut png = Vec::new();
        crate::app_icon::render(256, false).write_png_fast(&mut png).unwrap();
        for preset in PRESETS {
            let page = preset.engine().search(&png).unwrap();
            println!("{preset:?}: {page}");
            match preset {
                // The browser uploads to Lens; the page is what it opens.
                Preset::Google => assert!(fs::read_to_string(&page).unwrap().contains("lens.google.com/v3/upload")),
                Preset::Yandex => assert!(page.starts_with("https://")),
            }
        }
    }

    #[test]
    fn presets_are_valid_and_menu_ids_map_back() {
        for preset in PRESETS {
            assert_eq!(preset.engine().problem(), None);
        }
        assert!(engine("ftp://x", "", "").problem().unwrap().contains("上传地址"));
        let engines = || vec![Preset::Google.engine(), Preset::Yandex.engine()];
        let Some(Pick::Engines(one)) = picked(11, 10, engines()) else { panic!() };
        assert_eq!(one, vec![Preset::Yandex.engine()]);
        let Some(Pick::Engines(all)) = picked(10 + MENU_ALL, 10, engines()) else { panic!() };
        assert_eq!(all.len(), 2);
        assert!(matches!(picked(10 + MENU_SETTINGS, 10, engines()), Some(Pick::Settings)));
        assert!(picked(12, 10, engines()).is_none() && picked(3, 10, engines()).is_none());
        assert_eq!(busy_label(&engines()[..1]), "正在用 Google 识图…");
    }
}
