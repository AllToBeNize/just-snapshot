//! Updates from GitHub Releases. Every release carries `latest.json`: the
//! version, the release notes, the executable's URL, SHA-256 and size, signed
//! by a trusted publishing key (`release_key`). The app reads it from
//! `releases/latest/download/latest.json`, then from each mirror prefix in
//! `update.txt` when GitHub cannot be reached, so a mirror that rewrites the
//! file or the executable is caught by the signature and the hash.
//!
//! Installing renames the running executable to `just-snapshot.exe.old`
//! (Windows allows renaming a running program), copies the new one into its
//! place, starts it with `--after <pid>` and exits. The new process waits for
//! the old one, deletes the `.old` file and shows what changed. Any failure
//! before the new process starts puts the old executable back.

use std::{
    collections::VecDeque,
    env, fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Mutex,
    },
    thread,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND},
    System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

use crate::{digest, http, migrate, release_key};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPOSITORY: &str = "https://github.com/AllToBeNize/just-snapshot";
/// Posted to the tray window whenever `take_events` has something new.
pub const UPDATE_EVENT: u32 = WM_APP + 20;
const MANIFEST_LIMIT: usize = 256 * 1024;
const MAX_EXECUTABLE: u64 = 200 * 1024 * 1024;
/// How long the new process waits for the old one to exit.
const HANDOVER_MS: u32 = 15_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// What changed, for people: one item per line, `- ` starts a bullet.
    pub notes: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

pub enum Event {
    /// A check finished: `Ok(None)` means this is the newest version.
    Checked { manual: bool, result: Result<Option<Release>, String> },
    Progress { done: u64, total: u64 },
    Downloaded(Result<PathBuf, String>),
}

static EVENTS: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
static TARGET: AtomicIsize = AtomicIsize::new(0);
static CHECKING: AtomicBool = AtomicBool::new(false);
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// One line in `update/update.log`, so "the update never showed up" can be
/// traced afterwards: which check ran, what each source answered, how the
/// download and the swap went. Kept under 256 KB by starting over.
fn trace(line: &str) {
    // Tests must not write into the real log.
    if cfg!(test) {
        return;
    }
    let Ok(folder) = download_folder() else { return };
    let _ = fs::create_dir_all(&folder);
    let path = folder.join("update.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > 256 * 1024) {
        let _ = fs::remove_file(&path);
    }
    let mut time = windows_sys::Win32::Foundation::SYSTEMTIME::default();
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time) };
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} v{VERSION} {line}",
            time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
        );
    }
}

fn post(event: Event) {
    EVENTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(event);
    let window = TARGET.load(Ordering::Acquire) as HWND;
    if !window.is_null() {
        unsafe { PostMessageW(window, UPDATE_EVENT, 0, 0) };
    }
}

pub fn take_events() -> Vec<Event> {
    EVENTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain(..).collect()
}

/// Running from a Cargo `target` folder: a build being worked on, which an
/// update must never overwrite.
pub fn is_dev_build() -> bool {
    env::current_exe().is_ok_and(|path| {
        path.components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case("target"))
    })
}

/// `major.minor.patch`; anything else is not a version.
fn parse_version(text: &str) -> Option<[u32; 3]> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let version = [parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?];
    parts.next().is_none().then_some(version)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// Reads a signed `latest.json`. `verify(key, message, signature)` decides
/// whether the signature is trusted.
fn parse_manifest(
    bytes: &[u8],
    verify: impl Fn(&str, &[u8], &str) -> Result<(), String>,
) -> Result<Release, String> {
    let outer: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "更新信息不是有效的 JSON".to_string())?;
    let field = |value: &serde_json::Value, name: &str| {
        value
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| format!("更新信息缺少「{name}」"))
    };
    let signed = field(&outer, "signed")?;
    verify(&field(&outer, "key")?, signed.as_bytes(), &field(&outer, "signature")?)?;
    let inner: serde_json::Value =
        serde_json::from_str(&signed).map_err(|_| "更新信息内容不是有效的 JSON".to_string())?;
    let release = Release {
        version: field(&inner, "version")?,
        notes: field(&inner, "notes")?,
        url: field(&inner, "url")?,
        sha256: field(&inner, "sha256")?.to_ascii_lowercase(),
        size: inner.get("size").and_then(|size| size.as_u64()).ok_or("更新信息缺少「size」")?,
    };
    if parse_version(&release.version).is_none() {
        return Err(format!("更新信息里的版本号「{}」格式不对", release.version));
    }
    if !release.url.starts_with(&format!("{REPOSITORY}/releases/download/")) {
        return Err("更新信息里的下载地址不在 just-snapshot 的 GitHub 发布页".into());
    }
    if release.sha256.len() != 64 || !release.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("更新信息里的 SHA-256 格式不对".into());
    }
    if release.size == 0 || release.size > MAX_EXECUTABLE {
        return Err("更新信息里的文件大小不合理".into());
    }
    Ok(release)
}

/// `update.txt`: optional mirror prefixes and the version the user skipped.
#[derive(Default, Debug, PartialEq, Eq)]
struct Settings {
    /// Prefixes put in front of GitHub URLs, e.g. `https://mirror.example/`.
    mirrors: Vec<String>,
    skipped: Option<String>,
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(migrate::app_data()?.join("update.txt"))
}

impl Settings {
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "mirrors" => {
                    settings.mirrors = value
                        .split(';')
                        .map(str::trim)
                        .filter(|mirror| mirror.starts_with("https://") || mirror.starts_with("http://"))
                        .map(str::to_owned)
                        .collect();
                }
                "skip" if !value.trim().is_empty() => settings.skipped = Some(value.trim().into()),
                _ => {}
            }
        }
        settings
    }

    fn load() -> Self {
        settings_path()
            .and_then(|path| fs::read_to_string(path).map_err(|error| error.to_string()))
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let path = settings_path()?;
        fs::create_dir_all(path.parent().expect("settings folder")).map_err(|error| error.to_string())?;
        let text = format!(
            "# Prefixes tried in front of GitHub URLs when GitHub cannot be reached, separated by ;\r\nmirrors={}\r\nskip={}\r\n",
            self.mirrors.join(";"),
            self.skipped.as_deref().unwrap_or("")
        );
        fs::write(path, text).map_err(|error| format!("无法保存更新设置。\n{error}"))
    }

    /// GitHub first, then every mirror in front of the same URL.
    fn sources(&self, url: &str) -> Vec<(String, String)> {
        std::iter::once(("GitHub".to_string(), url.to_string()))
            .chain(self.mirrors.iter().map(|mirror| {
                let host = mirror.split('/').nth(2).unwrap_or(mirror).to_string();
                (format!("镜像 {host}"), format!("{mirror}{url}"))
            }))
            .collect()
    }
}

/// Where to fetch a file from the GitHub releases: GitHub, then each mirror
/// from `update.txt`, as `(name, url)`.
pub fn sources(url: &str) -> Vec<(String, String)> {
    Settings::load().sources(url)
}

/// Never offer `version` again on automatic checks.
pub fn skip(version: &str) -> Result<(), String> {
    let mut settings = Settings::load();
    settings.skipped = Some(version.into());
    settings.save()
}

fn fetch_release(settings: &Settings) -> Result<Release, String> {
    let sources = settings.sources(&format!("{REPOSITORY}/releases/latest/download/latest.json"));
    fetch_from(&sources, release_key::verify)
}

/// The first manifest among `sources` that parses and verifies.
fn fetch_from(
    sources: &[(String, String)],
    verify: impl Fn(&str, &[u8], &str) -> Result<(), String> + Copy,
) -> Result<Release, String> {
    let mut failures = Vec::new();
    for (name, url) in sources {
        let attempt = http::get(url, MANIFEST_LIMIT).and_then(|(status, body)| match status {
            200 => parse_manifest(&body, verify),
            404 => Err("还没有带更新信息的正式版本".into()),
            status => Err(format!("服务器返回 HTTP {status}")),
        });
        match attempt {
            Ok(release) => {
                trace(&format!("manifest {name}: v{} {url}", release.version));
                return Ok(release);
            }
            Err(error) => {
                trace(&format!("manifest {name}: {error} {url}"));
                failures.push(format!("{name}：{error}"));
            }
        }
    }
    Err(format!("检查更新失败。\n{}", failures.join("\n")))
}

/// Asks the update events to go to `window` (the tray window).
pub fn set_target(window: HWND) {
    TARGET.store(window as isize, Ordering::Release);
}

/// Checks for a newer release in the background. Automatic checks stay
/// quiet about errors and skip the version the user skipped.
pub fn check(manual: bool) {
    if CHECKING.swap(true, Ordering::AcqRel) {
        return;
    }
    trace(if manual { "check (manual)" } else { "check (automatic)" });
    let spawned = thread::Builder::new().name("snapshot-update-check".into()).spawn(move || {
        let settings = Settings::load();
        let result = fetch_release(&settings).map(|release| {
            let skipped = !manual && settings.skipped.as_deref() == Some(release.version.as_str());
            (is_newer(&release.version, VERSION) && !skipped).then_some(release)
        });
        CHECKING.store(false, Ordering::Release);
        trace(&match &result {
            Ok(Some(release)) => format!("offer v{}", release.version),
            Ok(None) => "up to date or skipped".into(),
            Err(_) => "check failed".into(),
        });
        post(Event::Checked { manual, result });
    });
    if spawned.is_err() {
        CHECKING.store(false, Ordering::Release);
    }
}

fn download_folder() -> Result<PathBuf, String> {
    Ok(migrate::app_data()?.join("update"))
}

/// Downloads and verifies `release` in the background, reporting progress.
pub fn download(release: Release) {
    if DOWNLOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    CANCEL.store(false, Ordering::Release);
    let spawned = thread::Builder::new().name("snapshot-update-download".into()).spawn(move || {
        let result = download_now(&release);
        DOWNLOADING.store(false, Ordering::Release);
        post(Event::Downloaded(result));
    });
    if spawned.is_err() {
        DOWNLOADING.store(false, Ordering::Release);
        post(Event::Downloaded(Err("无法开始下载".into())));
    }
}

pub fn cancel_download() {
    CANCEL.store(true, Ordering::Release);
}

fn download_now(release: &Release) -> Result<PathBuf, String> {
    let folder = download_folder()?;
    download_from(release, &Settings::load().sources(&release.url), &folder)
}

/// Downloads `release` from the first of `sources` that delivers the exact
/// file, into `folder`.
fn download_from(release: &Release, sources: &[(String, String)], folder: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(folder).map_err(|error| format!("无法创建下载文件夹。\n{error}"))?;
    let target = folder.join(format!("just-snapshot-{}.exe", release.version));
    let partial = target.with_extension("part");
    let mut failures = Vec::new();
    for (name, url) in sources {
        let mut last = u64::MAX;
        let attempt = fs::File::create(&partial)
            .map_err(|error| format!("无法写入下载文件夹：{error}"))
            .and_then(|file| {
                let mut output = BufWriter::new(file);
                let size = http::download(url, &mut output, |done, total| {
                    let total = total.unwrap_or(release.size).max(1);
                    // About two hundred progress events for the whole file.
                    if done * 200 / total != last {
                        last = done * 200 / total;
                        post(Event::Progress { done, total });
                    }
                    !CANCEL.load(Ordering::Acquire)
                })?;
                output.flush().map_err(|error| format!("写入下载的文件失败：{error}"))?;
                Ok(size)
            })
            .and_then(|_| verify_file(&partial, release));
        trace(&format!("download {name}: {} {url}", attempt.as_ref().err().map_or("ok", String::as_str)));
        match attempt {
            Ok(()) => {
                let _ = fs::remove_file(&target);
                fs::rename(&partial, &target).map_err(|error| format!("无法整理下载的文件。\n{error}"))?;
                return Ok(target);
            }
            Err(error) => {
                let _ = fs::remove_file(&partial);
                if error == http::CANCELLED {
                    return Err(error);
                }
                failures.push(format!("{name}：{error}"));
            }
        }
    }
    Err(format!("下载新版本失败。\n{}", failures.join("\n")))
}

fn verify_file(path: &Path, release: &Release) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("无法读取下载的文件：{error}"))?;
    if bytes.len() as u64 != release.size {
        return Err(format!("文件大小不对（{} 字节，应为 {}）", bytes.len(), release.size));
    }
    if digest::hex(&digest::sha256_bytes(&bytes)) != release.sha256 {
        return Err("文件校验不通过（SHA-256 不一致），可能下载出错或被篡改".into());
    }
    Ok(())
}

fn marker_path() -> Result<PathBuf, String> {
    Ok(download_folder()?.join("updated.json"))
}

fn old_path(current: &Path) -> PathBuf {
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    current.with_file_name(name)
}

/// Puts `downloaded` in place of the running executable and starts it.
/// On success the caller must exit right away.
pub fn install(downloaded: &Path, release: &Release) -> Result<(), String> {
    let current = env::current_exe().map_err(|error| format!("无法获取程序路径。\n{error}"))?;
    let marker = marker_path()?;
    let note = serde_json::json!({ "from": VERSION, "to": release.version, "notes": release.notes });
    fs::write(&marker, note.to_string()).map_err(|error| format!("无法记录更新内容。\n{error}"))?;
    let result = replace_and_start(&current, downloaded, |path| {
        Command::new(path)
            .arg("--after")
            .arg(std::process::id().to_string())
            .spawn()
            .map(drop)
            .map_err(|error| format!("无法启动新版本。\n{error}"))
    });
    if result.is_err() {
        let _ = fs::remove_file(&marker);
    }
    trace(&format!("install v{}: {}", release.version, result.as_ref().err().map_or("ok, restarting", String::as_str)));
    result
}

/// The swap itself, with the start of the new program injected for tests.
fn replace_and_start(
    current: &Path,
    downloaded: &Path,
    start: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let old = old_path(current);
    if old.exists() {
        fs::remove_file(&old).map_err(|error| format!("无法删除上次更新留下的 {}。\n{error}", old.display()))?;
    }
    fs::rename(current, &old).map_err(|error| format!("无法替换正在运行的程序。\n{error}"))?;
    let restore = |error: String| {
        let _ = fs::remove_file(current);
        match fs::rename(&old, current) {
            Ok(()) => error,
            Err(rollback) => format!("{error}\n旧版本没能恢复，请把 {} 改回 {}。\n{rollback}", old.display(), current.display()),
        }
    };
    if let Err(error) = fs::copy(downloaded, current) {
        return Err(restore(format!("无法放入新版本。\n{error}")));
    }
    start(current).map_err(restore)
}

/// What a finished update brought, for the card shown after the restart.
#[derive(Clone)]
pub struct Updated {
    pub from: String,
    pub version: String,
    pub notes: String,
}

/// Called at start-up. With `--after <pid>`, waits for the previous process
/// to exit so its tray icon and shortcuts are gone. Cleans up after an
/// update and returns what it brought, once.
pub fn finish(args: &[String]) -> Option<Updated> {
    if let Some(pid) = args
        .iter()
        .position(|arg| arg == "--after")
        .and_then(|index| args.get(index + 1))
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !process.is_null() {
                // If the old process hangs, carry on; its shortcuts may clash.
                WaitForSingleObject(process, HANDOVER_MS);
                CloseHandle(process);
            }
        }
    }
    if let Ok(current) = env::current_exe() {
        let _ = fs::remove_file(old_path(&current));
    }
    let marker = marker_path().ok()?;
    let text = fs::read_to_string(&marker).ok()?;
    let _ = fs::remove_file(&marker);
    if let Ok(folder) = download_folder() {
        if let Ok(entries) = fs::read_dir(folder) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "exe" || ext == "part") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    trace("started after an update");
    let note: serde_json::Value = serde_json::from_str(&text).ok()?;
    let version = note.get("to")?.as_str()?.to_string();
    // Only the version that was installed reports it.
    (version == VERSION).then(|| Updated {
        from: note.get("from").and_then(|from| from.as_str()).unwrap_or("").into(),
        version,
        notes: note.get("notes").and_then(|notes| notes.as_str()).unwrap_or("").into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(inner: &serde_json::Value) -> Vec<u8> {
        serde_json::json!({ "signed": inner.to_string(), "key": "pc", "signature": "c2ln" })
            .to_string()
            .into_bytes()
    }

    fn inner() -> serde_json::Value {
        serde_json::json!({
            "version": "0.5.0",
            "notes": "- 修好了长截图",
            "url": format!("{REPOSITORY}/releases/download/v0.5.0/just-snapshot.exe"),
            "sha256": "ab".repeat(32),
            "size": 1234,
        })
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("0.4.10", "0.4.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.4.1", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
        assert!(!is_newer("0.5", "0.4.1"), "not a version");
        assert!(!is_newer("0.5.0-beta", "0.4.1"), "not a version");
    }

    #[test]
    fn a_manifest_is_read_only_after_its_signature_checks_out() {
        let accept = |key: &str, _: &[u8], _: &str| (key == "pc").then_some(()).ok_or("bad".to_string());
        let release = parse_manifest(&manifest(&inner()), accept).unwrap();
        assert_eq!(release.version, "0.5.0");
        assert_eq!(release.size, 1234);
        let reject = |_: &str, _: &[u8], _: &str| Err("签名对不上".to_string());
        assert_eq!(parse_manifest(&manifest(&inner()), reject).unwrap_err(), "签名对不上");
    }

    #[test]
    fn a_signed_manifest_still_cannot_point_elsewhere() {
        let accept = |_: &str, _: &[u8], _: &str| Ok(());
        let mut elsewhere = inner();
        elsewhere["url"] = "https://example.com/just-snapshot.exe".into();
        assert!(parse_manifest(&manifest(&elsewhere), accept).is_err());
        let mut bad_hash = inner();
        bad_hash["sha256"] = "xyz".into();
        assert!(parse_manifest(&manifest(&bad_hash), accept).is_err());
        assert!(parse_manifest(b"not json", accept).is_err());
    }

    #[test]
    fn mirrors_are_tried_after_github() {
        let settings = Settings::parse("mirrors=https://mirror.example/; ftp://nope ;https://b.example/x/\r\nskip=0.4.2\r\n");
        assert_eq!(settings.skipped.as_deref(), Some("0.4.2"));
        let sources = settings.sources("https://github.com/a");
        assert_eq!(
            sources,
            vec![
                ("GitHub".to_string(), "https://github.com/a".to_string()),
                ("镜像 mirror.example".to_string(), "https://mirror.example/https://github.com/a".to_string()),
                ("镜像 b.example".to_string(), "https://b.example/x/https://github.com/a".to_string()),
            ]
        );
        assert_eq!(Settings::parse(""), Settings::default());
    }

    /// A local server: `/<name>` answers with the body for that name, `/down` closes the connection.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::{Read, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let read = stream.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..read]).lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                if path.starts_with("/down") {
                    continue;
                }
                let found = files.iter().find(|(name, _)| path.starts_with(&format!("/{name}")));
                let (status, body) = match found {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"missing".to_vec()),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn fetches_and_downloads_through_the_first_mirror_that_delivers_the_real_file() {
        let program = (0..300_000u32).map(|index| (index * 31 % 251) as u8).collect::<Vec<_>>();
        let mut tampered = program.clone();
        tampered[1000] ^= 1;
        let mut inner = inner();
        inner["sha256"] = digest::hex(&digest::sha256_bytes(&program)).into();
        inner["size"] = program.len().into();
        let base = serve(vec![
            ("good/latest.json", manifest(&inner)),
            ("bad/exe", tampered),
            ("good/exe", program.clone()),
        ]);
        let accept = |key: &str, _: &[u8], _: &str| (key == "pc").then_some(()).ok_or("bad".to_string());
        let source = |name: &str, path: &str| (name.to_string(), format!("{base}/{path}"));

        let manifests = [source("GitHub", "down"), source("空镜像", "missing"), source("好镜像", "good/latest.json")];
        let release = fetch_from(&manifests, accept).unwrap();
        assert_eq!(release.size, program.len() as u64);
        let error = fetch_from(&manifests[..2], accept).unwrap_err();
        assert!(error.contains("GitHub：") && error.contains("空镜像：还没有带更新信息的正式版本"), "{error}");

        let folder = env::temp_dir().join(format!("snapshot-download-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let downloads = [source("GitHub", "down"), source("坏镜像", "bad/exe"), source("好镜像", "good/exe")];
        let path = download_from(&release, &downloads, &folder).unwrap();
        assert_eq!(fs::read(&path).unwrap(), program);
        let error = download_from(&release, &downloads[..2], &folder).unwrap_err();
        assert!(error.contains("坏镜像：文件校验不通过"), "{error}");
        assert!(!path.with_extension("part").exists(), "no partial file is left behind");
        fs::remove_dir_all(&folder).unwrap();
    }

    /// Checks a real `latest.json` against the built-in keys:
    /// `SNAPSHOT_MANIFEST=<file> <test binary> verify_manifest_file --ignored --nocapture`.
    #[test]
    #[ignore]
    fn verify_manifest_file() {
        let path = env::var("SNAPSHOT_MANIFEST").unwrap();
        let release = parse_manifest(&fs::read(path).unwrap(), release_key::verify).unwrap();
        println!("verified v{} ({} bytes, sha256 {})", release.version, release.size, release.sha256);
        let mut tampered = fs::read(env::var("SNAPSHOT_MANIFEST").unwrap()).unwrap();
        let at = tampered.windows(5).position(|window| window == b"0.5.0").unwrap();
        tampered[at + 4] = b'1';
        println!("tampered: {}", parse_manifest(&tampered, release_key::verify).unwrap_err());
    }

    /// Fetches a URL with the app's own HTTP code:
    /// `SNAPSHOT_PROBE=<url> <test binary> probe_url --ignored --nocapture`.
    #[test]
    #[ignore]
    fn probe_url() {
        let url = env::var("SNAPSHOT_PROBE").unwrap();
        let started = std::time::Instant::now();
        let result = http::get(&url, MANIFEST_LIMIT).map(|(status, body)| (status, body.len()));
        println!("{result:?} in {:?}", started.elapsed());
    }

    #[test]
    fn a_failed_start_puts_the_old_program_back() {
        let folder = env::temp_dir().join(format!("snapshot-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        let current = folder.join("just-snapshot.exe");
        let downloaded = folder.join("new.exe");
        fs::write(&current, b"old").unwrap();
        fs::write(&downloaded, b"new").unwrap();

        let error = replace_and_start(&current, &downloaded, |_| Err("启动失败".into())).unwrap_err();
        assert!(error.starts_with("启动失败"));
        assert_eq!(fs::read(&current).unwrap(), b"old");
        assert!(!old_path(&current).exists());

        replace_and_start(&current, &downloaded, |path| {
            assert_eq!(fs::read(path).unwrap(), b"new");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert_eq!(fs::read(old_path(&current)).unwrap(), b"old");
        // The next update clears the previous leftover first.
        fs::write(&downloaded, b"newer").unwrap();
        replace_and_start(&current, &downloaded, |_| Ok(())).unwrap();
        assert_eq!(fs::read(old_path(&current)).unwrap(), b"new");
        fs::remove_dir_all(&folder).unwrap();
    }
}
