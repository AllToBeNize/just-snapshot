//! Turns a finished recording into a GIF for chats: Media Foundation decodes
//! the MP4, frames are taken at 10 fps, shrunk to at most 720 px wide and
//! written as a looping GIF with a palette per frame.

use std::{fs::File, io::BufWriter, path::Path};
use windows::{
    core::HSTRING,
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};

const FPS: i64 = 10;
const MAX_WIDTH: u32 = 720;
/// 100-ns units per second.
const TICKS: i64 = 10_000_000;
const FIRST_VIDEO: u32 = 0xFFFF_FFFC;
const ALL_STREAMS: u32 = 0xFFFF_FFFE;
const MEDIA_SOURCE: u32 = 0xFFFF_FFFF;

/// Converts `video` to `gif`, calling `progress` with 0.0–1.0 as it goes.
pub fn convert(video: &Path, gif: &Path, mut progress: impl FnMut(f32)) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let started = MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|error| format!("无法启动 Windows 媒体组件。\n{}", error.message()));
        let result = started.and_then(|()| convert_started(video, gif, &mut progress));
        let _ = MFShutdown();
        CoUninitialize();
        if result.is_err() {
            let _ = std::fs::remove_file(gif);
        }
        result
    }
}

unsafe fn convert_started(video: &Path, gif: &Path, progress: &mut dyn FnMut(f32)) -> Result<(), String> {
    let failed = |error: windows::core::Error| format!("无法读取录像。\n{}", error.message());
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 1).map_err(failed)?;
    let attributes = attributes.unwrap();
    attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(failed)?;
    let reader = MFCreateSourceReaderFromURL(&HSTRING::from(video.as_os_str()), &attributes).map_err(failed)?;
    reader.SetStreamSelection(ALL_STREAMS, false).map_err(failed)?;
    reader.SetStreamSelection(FIRST_VIDEO, true).map_err(failed)?;
    let wanted = MFCreateMediaType().map_err(failed)?;
    wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(failed)?;
    wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(failed)?;
    reader.SetCurrentMediaType(FIRST_VIDEO, None, &wanted).map_err(failed)?;
    let current = reader.GetCurrentMediaType(FIRST_VIDEO).map_err(failed)?;
    let size = current.GetUINT64(&MF_MT_FRAME_SIZE).map_err(failed)?;
    let (width, height) = ((size >> 32) as u32, (size & 0xffff_ffff) as u32);
    let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|stride| stride as i32).unwrap_or((width * 4) as i32);
    let duration = reader
        .GetPresentationAttribute(MEDIA_SOURCE, &MF_PD_DURATION)
        .ok()
        .and_then(|value| u64::try_from(&value).ok())
        .unwrap_or(0) as i64;
    let scale = (f64::from(MAX_WIDTH) / f64::from(width)).min(1.0);
    let (out_width, out_height) = (((f64::from(width) * scale).round() as u16).max(1), ((f64::from(height) * scale).round() as u16).max(1));
    let file = File::create(gif).map_err(|error| format!("无法创建 GIF 文件。\n{}\n{error}", gif.display()))?;
    let mut encoder = gif::Encoder::new(BufWriter::new(file), out_width, out_height, &[])
        .map_err(|error| format!("无法写入 GIF。\n{error}"))?;
    encoder.set_repeat(gif::Repeat::Infinite).map_err(|error| format!("无法写入 GIF。\n{error}"))?;
    let step = TICKS / FPS;
    let mut next = 0i64;
    let mut written = 0;
    loop {
        let (mut flags, mut time, mut sample) = (0u32, 0i64, None);
        reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).map_err(failed)?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(sample) = sample else { continue };
        if time < next {
            continue;
        }
        next = (time / step + 1) * step;
        let buffer = sample.ConvertToContiguousBuffer().map_err(failed)?;
        let mut data = std::ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut data, None, Some(&mut length)).map_err(failed)?;
        let pixels = std::slice::from_raw_parts(data, length as usize);
        let mut rgba = shrink(pixels, width, height, stride, out_width.into(), out_height.into());
        let _ = buffer.Unlock();
        let mut frame = gif::Frame::from_rgba_speed(out_width, out_height, &mut rgba, 10);
        frame.delay = (100 / FPS) as u16;
        encoder.write_frame(&frame).map_err(|error| format!("无法写入 GIF。\n{error}"))?;
        written += 1;
        if duration > 0 {
            progress((time as f32 / duration as f32).clamp(0.0, 1.0));
        }
    }
    if written == 0 {
        return Err("录像里没有画面，没法生成 GIF。".into());
    }
    progress(1.0);
    Ok(())
}

/// BGRA rows (bottom-up when `stride` is negative) averaged down to RGBA.
fn shrink(pixels: &[u8], width: u32, height: u32, stride: i32, out_width: u32, out_height: u32) -> Vec<u8> {
    let row_bytes = stride.unsigned_abs() as usize;
    let row = |y: u32| if stride < 0 { (height - 1 - y) as usize * row_bytes } else { y as usize * row_bytes };
    let mut out = Vec::with_capacity((out_width * out_height * 4) as usize);
    for oy in 0..out_height {
        let (y0, y1) = (oy * height / out_height, ((oy + 1) * height / out_height).max(oy * height / out_height + 1));
        for ox in 0..out_width {
            let (x0, x1) = (ox * width / out_width, ((ox + 1) * width / out_width).max(ox * width / out_width + 1));
            let mut sum = [0u32; 3];
            let mut count = 0;
            for y in y0..y1.min(height) {
                let start = row(y);
                for x in x0..x1.min(width) {
                    let index = start + x as usize * 4;
                    if index + 2 < pixels.len() {
                        sum[0] += u32::from(pixels[index + 2]);
                        sum[1] += u32::from(pixels[index + 1]);
                        sum[2] += u32::from(pixels[index]);
                        count += 1;
                    }
                }
            }
            let count = count.max(1);
            out.extend([(sum[0] / count) as u8, (sum[1] / count) as u8, (sum[2] / count) as u8, 255]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinking_averages_and_reads_bottom_up_rows() {
        // 2×2 BGRA, bottom-up: the first row in memory is the bottom one.
        let pixels = [0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255, 255, 0, 0, 255];
        let top_down = shrink(&pixels, 2, 2, 8, 2, 2);
        assert_eq!(&top_down[..4], &[255, 0, 0, 255], "top-down: red on top");
        let bottom_up = shrink(&pixels, 2, 2, -8, 2, 2);
        assert_eq!(&bottom_up[..4], &[0, 0, 255, 255], "bottom-up: blue on top");
        let averaged = shrink(&pixels, 2, 2, 8, 1, 1);
        assert_eq!(averaged, vec![127, 0, 127, 255]);
    }

    /// A frame from a recording as PNG, to look at what was recorded. Records
    /// SNAPSHOT_TEST_SIZE at SNAPSHOT_TEST_QUALITY for a second. Run with
    /// `<test binary> frame_preview --ignored`.
    #[test]
    #[ignore]
    fn frame_preview() {
        use crate::recorder::{start, Command, Options, Quality};
        let size = std::env::var("SNAPSHOT_TEST_SIZE").unwrap_or("2560x1440".into());
        let (width, height) = size.split_once('x').map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap())).unwrap();
        let quality = std::env::var("SNAPSHOT_TEST_QUALITY").ok().and_then(|key| Quality::ALL.into_iter().find(|q| q.key() == key)).unwrap_or_default();
        let video = std::env::temp_dir().join(format!("snapshot-frame-{}.mp4", std::process::id()));
        let area = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: width, bottom: height };
        let recording = start(area, &video, Options { quality, ..Options::default() }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(800));
        recording.send(Command::Stop);
        recording.finish().unwrap();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1).unwrap();
            let attributes = attributes.unwrap();
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).unwrap();
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(video.as_os_str()), &attributes).unwrap();
            let wanted = MFCreateMediaType().unwrap();
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).unwrap();
            reader.SetCurrentMediaType(FIRST_VIDEO, None, &wanted).unwrap();
            let current = reader.GetCurrentMediaType(FIRST_VIDEO).unwrap();
            let frame = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
            let (w, h) = ((frame >> 32) as u32, (frame & 0xffff_ffff) as u32);
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((w * 4) as i32);
            let mut picked = None;
            for _ in 0..20 {
                let (mut flags, mut time, mut sample) = (0u32, 0i64, None);
                reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).unwrap();
                if let Some(sample) = sample {
                    picked = Some(sample);
                }
            }
            let buffer = picked.unwrap().ConvertToContiguousBuffer().unwrap();
            let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut data, None, Some(&mut length)).unwrap();
            let rgba = shrink(std::slice::from_raw_parts(data, length as usize), w, h, stride, w / 2, h / 2);
            let _ = buffer.Unlock();
            let mut pixels = Vec::with_capacity(rgba.len());
            for px in rgba.chunks_exact(4) {
                pixels.extend([px[2], px[1], px[0], 255]);
            }
            let image = crate::capture::Image { width: (w / 2) as i32, height: (h / 2) as i32, pixels };
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            image.save_png(&folder.join(format!("recorded-{size}-{}.png", quality.key()))).unwrap();
            println!("video {w}x{h}");
        }
        let _ = std::fs::remove_file(&video);
    }

    /// Records a second and converts it. Run with `<test binary> gif_export --ignored --nocapture`.
    #[test]
    #[ignore]
    fn converts_a_recording() {
        use crate::recorder::{start, Command, Options};
        let folder = std::env::temp_dir();
        let video = folder.join(format!("snapshot-gif-test-{}.mp4", std::process::id()));
        let gif = video.with_extension("gif");
        let area = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: 400, bottom: 300 };
        let recording = start(area, &video, Options { fps: 30, system_audio: false, cursor: false, ..Options::default() }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        recording.send(Command::Stop);
        recording.finish().unwrap();
        let mut steps = Vec::new();
        convert(&video, &gif, |done| steps.push(done)).unwrap();
        let bytes = std::fs::read(&gif).unwrap();
        assert_eq!(&bytes[..6], b"GIF89a");
        assert!(steps.len() >= 10, "about ten frames: {}", steps.len());
        assert_eq!(steps.last(), Some(&1.0));
        let _ = std::fs::remove_file(&video);
        let _ = std::fs::remove_file(&gif);
    }
}
