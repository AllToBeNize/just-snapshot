//! The text recognition setting (快速: Windows OCR, 准确: PaddleOCR) and the
//! download of the accurate engine's pack: ONNX Runtime and the PP-OCRv5
//! models, about 37 MB, kept out of the executable so updates stay small.
//! Each file comes from the `ocr-pack-v1` release (or a mirror from
//! `update.txt`) and must match its size and SHA-256 before it is used.

use std::{
    fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use crate::{digest, http, paddle_ocr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Engine {
    /// The recognizer built into Windows: instant, weaker on small and inverted text.
    Fast,
    /// PaddleOCR from the downloaded pack.
    Accurate,
}

pub struct PackFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const FILES: [PackFile; 3] = [
    PackFile {
        name: paddle_ocr::RUNTIME,
        size: 17_253_176,
        sha256: "0c42dc4e278f0968fdf9ebf1b1e54a74c657a9942e70803e453c91993451cf4c",
    },
    PackFile {
        name: paddle_ocr::DETECTOR,
        size: 4_819_576,
        sha256: "4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae",
    },
    PackFile {
        name: paddle_ocr::RECOGNIZER,
        size: 16_631_306,
        sha256: "5825fc7ebf84ae7a412be049820b4d86d77620f204a041697b0494669b1742c5",
    },
];

const RELEASE: &str = "https://github.com/AllToBeNize/just-snapshot/releases/download/ocr-pack-v1/";

pub fn total_size() -> u64 {
    FILES.iter().map(|file| file.size).sum()
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
    FILES
        .iter()
        .all(|file| fs::metadata(folder.join(file.name)).is_ok_and(|metadata| metadata.len() == file.size))
}

/// Downloads the missing files. `progress(done, total)` follows every chunk
/// and cancels by returning false.
pub fn install(mut progress: impl FnMut(u64, u64) -> bool) -> Result<(), String> {
    let folder = folder()?;
    install_from(&folder, |name| crate::update::sources(&format!("{RELEASE}{name}")), &mut progress)
}

fn install_from(
    folder: &Path,
    sources: impl Fn(&str) -> Vec<(String, String)>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|error| format!("无法创建识别包文件夹 {}。\n{error}", folder.display()))?;
    let total = total_size();
    let mut done = 0;
    for file in &FILES {
        let target = folder.join(file.name);
        if fs::metadata(&target).is_ok_and(|metadata| metadata.len() == file.size) {
            done += file.size;
            continue;
        }
        let partial = target.with_extension("part");
        let mut failures = Vec::new();
        let mut fetched = false;
        for (source, url) in sources(file.name) {
            let attempt = fs::File::create(&partial)
                .map_err(|error| format!("无法写入识别包文件夹：{error}"))
                .and_then(|output| {
                    let mut output = BufWriter::new(output);
                    http::download(&url, &mut output, |got, _| progress(done + got, total))?;
                    output.flush().map_err(|error| format!("写入下载的文件失败：{error}"))
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
        done += file.size;
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

fn verify(path: &Path, file: &PackFile) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("无法读取下载的文件：{error}"))?;
    if bytes.len() as u64 != file.size {
        return Err(format!("文件大小不对（{} 字节，应为 {}）", bytes.len(), file.size));
    }
    if digest::hex(&digest::sha256_bytes(&bytes)) != file.sha256 {
        return Err("文件校验不通过（SHA-256 不一致），可能下载出错或被篡改".into());
    }
    Ok(())
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
        install_from(&folder, |name| crate::update::sources(&format!("{RELEASE}{name}")), &mut |_, _| true).unwrap();
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
    fn a_file_that_fails_its_hash_is_not_kept() {
        let folder = std::env::temp_dir().join(format!("snapshot-ocr-pack-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let (address, _) = serve(vec![(200, "not the runtime".into())]);
        let error = install_from(&folder, |name| vec![("测试".into(), format!("{address}/{name}"))], &mut |_, _| true).unwrap_err();
        assert!(error.contains(paddle_ocr::RUNTIME) && error.contains("测试："), "{error}");
        assert!(!installed_in(&folder));
        assert!(fs::read_dir(&folder).unwrap().next().is_none(), "nothing partial is left behind");
        fs::remove_dir_all(&folder).unwrap();
    }
}
