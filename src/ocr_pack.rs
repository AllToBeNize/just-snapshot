//! The text recognition setting (快速: Windows OCR, 准确: PaddleOCR) and the
//! download of the accurate engine's pack: ONNX Runtime and the PP-OCRv5
//! models, about 37 MB, kept out of the executable so updates stay small.
//!
//! Sources reachable from China come first: the models from RapidAI's
//! ModelScope repository, the runtime out of Microsoft's onnxruntime wheel on
//! the Tsinghua and Aliyun PyPI mirrors. Then the `ocr-pack-v1` release on
//! GitHub and the mirrors from `update.txt`. Whatever the source, a file must
//! match one of its known builds (size and SHA-256) before it is used.

use std::{
    fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::PostMessageW,
};

use crate::{digest, http, paddle_ocr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Engine {
    /// The recognizer built into Windows: instant, weaker on small and inverted text.
    Fast,
    /// PaddleOCR from the downloaded pack.
    Accurate,
}

/// One accepted build of a pack file.
pub struct Build {
    pub size: u64,
    pub sha256: &'static str,
}

pub struct PackFile {
    pub name: &'static str,
    /// The builds the sources hand out; the first is the release's.
    pub builds: &'static [Build],
}

pub const FILES: [PackFile; 3] = [
    PackFile {
        name: paddle_ocr::RUNTIME,
        builds: &[
            // ONNX Runtime 1.31.0 from the NuGet package, on the release.
            Build { size: 17_253_176, sha256: "0c42dc4e278f0968fdf9ebf1b1e54a74c657a9942e70803e453c91993451cf4c" },
            // The same version from the Python wheel on PyPI.
            Build { size: 19_288_888, sha256: "7f6af4d294cf33d84dd66211d2d7a385e45108ac34716f1717dfbbd40e932952" },
        ],
    },
    PackFile {
        name: paddle_ocr::DETECTOR,
        builds: &[Build { size: 4_819_576, sha256: "4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae" }],
    },
    PackFile {
        name: paddle_ocr::RECOGNIZER,
        builds: &[Build { size: 16_631_306, sha256: "5825fc7ebf84ae7a412be049820b4d86d77620f204a041697b0494669b1742c5" }],
    },
];

const RELEASE: &str = "https://github.com/AllToBeNize/just-snapshot/releases/download/ocr-pack-v1/";
const MODELSCOPE: &str = "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.4.0/onnx/PP-OCRv5/";
/// Microsoft's onnxruntime 1.31.0 wheel, under each PyPI host's `packages/`.
const WHEEL: &str = "37/de/6cab7e39917cc87728d2f00abe97c81fe86b29f9e1f758627864c28f0c21/onnxruntime-1.31.0-cp311-cp311-win_amd64.whl";
const WHEEL_ENTRY: &str = "onnxruntime/capi/onnxruntime.dll";

/// How a source delivers a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The file itself.
    File(String),
    /// A wheel (a zip) with the file at `WHEEL_ENTRY`.
    Wheel(String),
}

pub fn total_size() -> u64 {
    FILES.iter().map(|file| file.builds[0].size).sum()
}

/// Where `file` can come from, in the order tried, each with a name for errors.
fn sources(file: &PackFile) -> Vec<(String, Fetch)> {
    let release = crate::update::sources(&format!("{RELEASE}{}", file.name))
        .into_iter()
        .map(|(name, url)| (name, Fetch::File(url)));
    let wheel = |host: &str| Fetch::Wheel(format!("{host}{WHEEL}"));
    match file.name {
        paddle_ocr::RUNTIME => [
            ("清华 PyPI 镜像".to_string(), wheel("https://pypi.tuna.tsinghua.edu.cn/packages/")),
            ("阿里云 PyPI 镜像".to_string(), wheel("https://mirrors.aliyun.com/pypi/packages/")),
        ]
        .into_iter()
        .chain(release)
        .chain([("PyPI".to_string(), wheel("https://files.pythonhosted.org/packages/"))])
        .collect(),
        name => {
            let folder = if name == paddle_ocr::DETECTOR { "det" } else { "rec" };
            std::iter::once(("魔搭 ModelScope".to_string(), Fetch::File(format!("{MODELSCOPE}{folder}/{name}"))))
                .chain(release)
                .collect()
        }
    }
}

/// Where the pack lives: `%LOCALAPPDATA%\just-snapshot\ocr\pp-ocrv5`.
pub fn folder() -> Result<PathBuf, String> {
    Ok(crate::migrate::app_data()?.join("ocr").join("pp-ocrv5"))
}

/// Every file is there with the right size (hashes are checked on download).
pub fn installed() -> bool {
    folder().is_ok_and(|folder| installed_in(&folder))
}

fn installed_in(folder: &Path) -> bool {
    FILES.iter().all(|file| present(folder, file))
}

/// The file is there in one of its builds' sizes.
fn present(folder: &Path, file: &PackFile) -> bool {
    fs::metadata(folder.join(file.name)).is_ok_and(|metadata| file.builds.iter().any(|build| build.size == metadata.len()))
}

/// Downloads the missing files. `progress(done, total)` follows every chunk
/// and cancels by returning false.
pub fn install(mut progress: impl FnMut(u64, u64) -> bool) -> Result<(), String> {
    let folder = folder()?;
    install_from(&folder, sources, &mut progress)
}

fn install_from(
    folder: &Path,
    sources: impl Fn(&PackFile) -> Vec<(String, Fetch)>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|error| format!("无法创建识别包文件夹 {}。\n{error}", folder.display()))?;
    let total = total_size();
    let mut done = 0;
    for file in &FILES {
        let target = folder.join(file.name);
        let share = file.builds[0].size;
        if present(folder, file) {
            done += share;
            continue;
        }
        let partial = target.with_extension("part");
        let mut failures = Vec::new();
        let mut fetched = false;
        for (source, fetch) in sources(file) {
            let url = match &fetch {
                Fetch::File(url) | Fetch::Wheel(url) => url.clone(),
            };
            let attempt = fs::File::create(&partial)
                .map_err(|error| format!("无法写入识别包文件夹：{error}"))
                .and_then(|output| {
                    let mut output = BufWriter::new(output);
                    // Progress counts each file as its share, whatever the source sends.
                    http::download(&url, &mut output, |got, size| {
                        let got = size.filter(|&size| size > 0).map_or(got.min(share), |size| got * share / size);
                        progress(done + got, total)
                    })?;
                    output.flush().map_err(|error| format!("写入下载的文件失败：{error}"))
                })
                .and_then(|()| match fetch {
                    Fetch::File(_) => Ok(()),
                    Fetch::Wheel(_) => {
                        let wheel = fs::read(&partial).map_err(|error| format!("无法读取下载的文件：{error}"))?;
                        let extracted = extract(&wheel, WHEEL_ENTRY)?;
                        fs::write(&partial, extracted).map_err(|error| format!("无法写入识别包文件夹：{error}"))
                    }
                })
                .and_then(|()| verify(&partial, file));
            match attempt {
                Ok(()) => {
                    let _ = fs::remove_file(&target);
                    fs::rename(&partial, &target).map_err(|error| format!("无法整理下载的文件。\n{error}"))?;
                    fetched = true;
                    break;
                }
                Err(error) => {
                    let _ = fs::remove_file(&partial);
                    if error == http::CANCELLED {
                        return Err(error);
                    }
                    failures.push(format!("{source}：{error}"));
                }
            }
        }
        if !fetched {
            return Err(format!("下载 {} 失败。\n{}", file.name, failures.join("\n")));
        }
        done += share;
    }
    // The licenses travel with what they cover.
    for (name, text) in NOTICES {
        fs::write(folder.join(name), text).map_err(|error| format!("无法写入识别包的许可证文件。\n{error}"))?;
    }
    Ok(())
}

const NOTICES: [(&str, &str); 2] = [
    ("onnxruntime-license.txt", include_str!("../licenses/onnxruntime.txt")),
    ("paddleocr-license.txt", include_str!("../licenses/paddleocr.txt")),
];

static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
/// The window that hears about the download; a reopened settings window takes over.
static TARGET: AtomicIsize = AtomicIsize::new(0);

/// Sends the running download's messages to `window` from now on.
pub fn retarget(window: HWND) {
    TARGET.store(window as isize, Ordering::Release);
}

pub fn is_downloading() -> bool {
    DOWNLOADING.load(Ordering::Acquire)
}

pub fn cancel_download() {
    CANCEL.store(true, Ordering::Release);
}

/// Downloads the pack in the background. Posts `progress` to `window` with
/// bytes done in `wparam` and the total in `lparam`, then `done` with an
/// owned `Box<Result<(), String>>` in `lparam`. False if one is already running.
pub fn start_download(window: HWND, progress: u32, done: u32) -> bool {
    if DOWNLOADING.swap(true, Ordering::AcqRel) {
        return false;
    }
    CANCEL.store(false, Ordering::Release);
    retarget(window);
    std::thread::spawn(move || {
        let target = || TARGET.load(Ordering::Acquire);
        let mut last = u64::MAX;
        let result = install(|got, total| {
            // About two hundred progress messages for the whole pack.
            if got * 200 / total.max(1) != last {
                last = got * 200 / total.max(1);
                unsafe { PostMessageW(target() as HWND, progress, got as WPARAM, total as LPARAM) };
            }
            !CANCEL.load(Ordering::Acquire)
        });
        DOWNLOADING.store(false, Ordering::Release);
        let result = Box::into_raw(Box::new(result));
        unsafe {
            if PostMessageW(target() as HWND, done, 0, result as LPARAM) == 0 {
                drop(Box::from_raw(result));
            }
        }
    });
    true
}

/// The file matches one of its known builds.
fn verify(path: &Path, file: &PackFile) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("无法读取下载的文件：{error}"))?;
    let Some(build) = file.builds.iter().find(|build| build.size == bytes.len() as u64) else {
        let sizes: Vec<String> = file.builds.iter().map(|build| build.size.to_string()).collect();
        return Err(format!("文件大小不对（{} 字节，应为 {}）", bytes.len(), sizes.join(" 或 ")));
    };
    if digest::hex(&digest::sha256_bytes(&bytes)) != build.sha256 {
        return Err("文件校验不通过（SHA-256 不一致），可能下载出错或被篡改".into());
    }
    Ok(())
}

/// `name` out of a zip archive (stored or deflated, no zip64), enough for a wheel.
fn extract(zip: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let damaged = || "下载的安装包不完整，无法解压".to_string();
    let u16_at = |at: usize| zip.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or_else(damaged);
    let u32_at = |at: usize| zip.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(damaged);
    // The end record sits in the last 64 KB + 22 bytes, after any comment.
    let lowest = zip.len().saturating_sub(22 + 65_535);
    let end = (lowest..=zip.len().saturating_sub(22))
        .rev()
        .find(|&at| zip[at..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(damaged)?;
    let entries = u16_at(end + 10)?;
    let mut at = u32_at(end + 16)?;
    for _ in 0..entries {
        if u32_at(at)? != 0x0201_4b50 {
            return Err(damaged());
        }
        let method = u16_at(at + 10)?;
        let (packed, size) = (u32_at(at + 20)?, u32_at(at + 24)?);
        let (name_length, extra, comment) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?);
        let local = u32_at(at + 42)?;
        let entry = zip.get(at + 46..at + 46 + name_length).ok_or_else(damaged)?;
        if entry == name.as_bytes() {
            if u32_at(local)? != 0x0403_4b50 {
                return Err(damaged());
            }
            let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let data = zip.get(start..start + packed).ok_or_else(damaged)?;
            return match method {
                0 => Ok(data.to_vec()),
                8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, size).map_err(|_| damaged()),
                _ => Err(format!("安装包用了不支持的压缩方式（{method}）")),
            };
        }
        at += 46 + name_length + extra + comment;
    }
    Err(format!("安装包里没有 {name}"))
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(crate::migrate::app_data()?.join("ocr.txt"))
}

/// The chosen engine; 快速 until the user picks 准确.
pub fn engine() -> Engine {
    settings_path()
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .map_or(Engine::Fast, |text| parse(&text))
}

fn parse(text: &str) -> Engine {
    let accurate = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .any(|(key, value)| key.trim() == "engine" && value.trim() == "accurate");
    if accurate {
        Engine::Accurate
    } else {
        Engine::Fast
    }
}

pub fn set_engine(engine: Engine) -> Result<(), String> {
    let path = settings_path()?;
    let failed = |error: io::Error| format!("无法保存文字识别设置 {}。\n{error}", path.display());
    fs::create_dir_all(path.parent().expect("settings folder")).map_err(failed)?;
    let value = match engine {
        Engine::Fast => "fast",
        Engine::Accurate => "accurate",
    };
    fs::write(&path, format!("# fast: Windows 自带识别；accurate: PaddleOCR（需要识别包）\r\nengine={value}\r\n")).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::tests::serve;

    #[test]
    fn the_setting_defaults_to_fast() {
        assert_eq!(parse(""), Engine::Fast);
        assert_eq!(parse("# x\r\nengine = accurate\r\n"), Engine::Accurate);
        assert_eq!(parse("engine=fast"), Engine::Fast);
    }

    /// Downloads the real pack from the release into a temporary folder,
    /// then recognizes with it: `<test binary> ocr_pack::tests::install_live --ignored --nocapture`.
    #[test]
    #[ignore]
    fn install_live() {
        let folder = std::env::temp_dir().join("snapshot-ocr-pack-live");
        let _ = fs::remove_dir_all(&folder);
        let start = std::time::Instant::now();
        install_from(&folder, sources, &mut |_, _| true).unwrap();
        println!("downloaded in {:?}", start.elapsed());
        assert!(installed_in(&folder));
        assert!(folder.join("paddleocr-license.txt").exists());
        let image = unsafe { crate::ocr::tests::sample("准确档 PaddleOCR UI_TabControl", 16, true) };
        let text = crate::ocr::text(&paddle_ocr::recognize(&image, &folder).unwrap());
        println!("{text}");
        assert!(text.contains("UI_TabControl"), "{text}");
        // The runtime stays loaded until the process ends, so it cannot be deleted yet.
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn china_reachable_sources_come_first() {
        let names = |file: &PackFile| sources(file).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        let runtime = names(&FILES[0]);
        assert_eq!(&runtime[..3], ["清华 PyPI 镜像", "阿里云 PyPI 镜像", "GitHub"]);
        assert_eq!(runtime.last().unwrap(), "PyPI");
        assert_eq!(&names(&FILES[1])[..2], ["魔搭 ModelScope", "GitHub"]);
        assert_eq!(
            sources(&FILES[2])[0].1,
            Fetch::File(format!("{MODELSCOPE}rec/{}", paddle_ocr::RECOGNIZER))
        );
        assert!(matches!(&sources(&FILES[0])[0].1, Fetch::Wheel(url) if url.ends_with("win_amd64.whl")));
    }

    #[test]
    fn a_wheel_gives_up_the_runtime() {
        // A zip with a stored and a deflated entry, as Python writes wheels.
        let deflated = miniz_oxide::deflate::compress_to_vec(b"runtime bytes runtime bytes", 6);
        let mut zip = Vec::new();
        let mut central = Vec::new();
        for (name, method, data, size) in [("a/stored.txt", 0u16, b"hello".to_vec(), 5u32), (WHEEL_ENTRY, 8, deflated, 27)] {
            let offset = zip.len() as u32;
            let header = |signature: u32| signature.to_le_bytes().to_vec();
            let mut local = header(0x0403_4b50);
            local.extend([20, 0, 0, 0]);
            local.extend(method.to_le_bytes());
            local.extend([0; 8]);
            local.extend((data.len() as u32).to_le_bytes());
            local.extend(size.to_le_bytes());
            local.extend((name.len() as u16).to_le_bytes());
            local.extend([0, 0]);
            local.extend(name.as_bytes());
            zip.extend(local);
            zip.extend(&data);
            let mut entry = header(0x0201_4b50);
            entry.extend([20, 0, 20, 0, 0, 0]);
            entry.extend(method.to_le_bytes());
            entry.extend([0; 8]);
            entry.extend((data.len() as u32).to_le_bytes());
            entry.extend(size.to_le_bytes());
            entry.extend((name.len() as u16).to_le_bytes());
            entry.extend([0; 12]);
            entry.extend(offset.to_le_bytes());
            entry.extend(name.as_bytes());
            central.extend(entry);
        }
        let central_offset = zip.len() as u32;
        let central_size = central.len() as u32;
        zip.extend(central);
        zip.extend(0x0605_4b50u32.to_le_bytes());
        zip.extend([0, 0, 0, 0, 2, 0, 2, 0]);
        zip.extend(central_size.to_le_bytes());
        zip.extend(central_offset.to_le_bytes());
        zip.extend([0, 0]);
        assert_eq!(extract(&zip, WHEEL_ENTRY).unwrap(), b"runtime bytes runtime bytes");
        assert_eq!(extract(&zip, "a/stored.txt").unwrap(), b"hello");
        assert!(extract(&zip, "missing").unwrap_err().contains("没有"));
        assert!(extract(&zip[..zip.len() - 30], WHEEL_ENTRY).is_err());
    }

    #[test]
    fn a_file_that_fails_its_hash_is_not_kept() {
        let folder = std::env::temp_dir().join(format!("snapshot-ocr-pack-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let (address, _) = serve(vec![(200, "not the runtime".into())]);
        let error = install_from(&folder, |file| vec![("测试".into(), Fetch::File(format!("{address}/{}", file.name)))], &mut |_, _| true).unwrap_err();
        assert!(error.contains(paddle_ocr::RUNTIME) && error.contains("测试："), "{error}");
        assert!(!installed_in(&folder));
        assert!(fs::read_dir(&folder).unwrap().next().is_none(), "nothing partial is left behind");
        fs::remove_dir_all(&folder).unwrap();
    }
}
