//! Debug recording of long-capture sessions. When the folder
//! `%LOCALAPPDATA%\just-snapshot\scroll-debug` exists, every session writes
//! its first frame, every distinct sampled frame and a log of wheel events
//! and stitch results into a subfolder named after the start time, so a
//! failing session can be replayed through `Stitcher` with
//! `scroll_stitch::tests::replay_dump`. Without the folder nothing runs.

use std::{
    fmt::Write as _,
    ptr::null_mut,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::mpsc::{self, Sender},
    thread,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, POINT, RECT, SYSTEMTIME},
    System::{
        SystemInformation::GetLocalTime,
        Threading::{
            GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    Graphics::Gdi::ScreenToClient,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

use crate::{capture::Image, migrate, scroll_stitch::Update, selection_geometry::contains};

/// Enough for a long session; past it only the log continues.
const MAX_FRAMES: usize = 400;

enum Job {
    Frame(String, Image),
    Line(String),
}

pub struct Dump {
    jobs: Sender<Job>,
    started: Instant,
    width: i32,
    height: i32,
    last: Vec<u8>,
    frames: usize,
    wheels: usize,
}

impl Dump {
    /// Starts recording when the debug folder exists.
    pub unsafe fn start(region: RECT, first: &Image) -> Option<Self> {
        let root = migrate::app_data().ok()?.join("scroll-debug");
        if !root.is_dir() {
            return None;
        }
        let mut time = SYSTEMTIME::default();
        GetLocalTime(&mut time);
        let folder = root.join(format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
        ));
        fs::create_dir_all(&folder).ok()?;
        let log = File::create(folder.join("log.txt")).ok()?;
        let (jobs, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("scroll-dump".into())
            .spawn(move || write_jobs(folder, log, receiver.into_iter()))
            .ok()?;
        let dump = Self {
            jobs,
            started: Instant::now(),
            width: first.width,
            height: first.height,
            last: first.pixels.clone(),
            frames: 0,
            wheels: 0,
        };
        dump.line(describe(region));
        dump.save("first".into(), first.pixels.clone());
        Some(dump)
    }

    pub fn wheel(&mut self) {
        self.wheels += 1;
        let line = format!("{} wheel #{}", self.elapsed(), self.wheels);
        self.line(line);
    }

    /// Logs one sampled frame and what the stitcher made of it; the frame
    /// itself is written only when it differs from the previous sample.
    pub fn frame(&mut self, frame: &[u8], update: Update, image_height: i32) {
        let changed = self.last != frame;
        let mut line = format!(
            "{} sample {:?} height={image_height}",
            self.elapsed(),
            update
        );
        if changed {
            self.last.clear();
            self.last.extend_from_slice(frame);
            self.frames += 1;
            if self.frames <= MAX_FRAMES {
                let name = format!("frame-{:04}", self.frames);
                let _ = write!(line, " -> {name}.png");
                self.save(name, frame.to_vec());
            } else {
                line.push_str(" (frame limit, not saved)");
            }
        } else {
            line.push_str(" (same as previous sample)");
        }
        self.line(line);
    }

    pub fn note(&self, text: &str) {
        self.line(format!("{} {text}", self.elapsed()));
    }

    fn elapsed(&self) -> String {
        format!("t={:>6}ms", self.started.elapsed().as_millis())
    }

    fn line(&self, line: String) {
        let _ = self.jobs.send(Job::Line(line));
    }

    fn save(&self, name: String, pixels: Vec<u8>) {
        let image = Image {
            width: self.width,
            height: self.height,
            pixels,
        };
        let _ = self.jobs.send(Job::Frame(name, image));
    }
}

fn write_jobs(folder: PathBuf, mut log: File, jobs: impl Iterator<Item = Job>) {
    for job in jobs {
        match job {
            Job::Line(line) => {
                let _ = writeln!(log, "{line}");
            }
            Job::Frame(name, image) => {
                let path = folder.join(format!("{name}.png"));
                let saved = File::create(&path)
                    .map_err(Into::into)
                    .and_then(|file| image.write_png_fast(std::io::BufWriter::new(file)));
                if let Err(error) = saved {
                    let _ = writeln!(log, "failed to save {name}.png: {error}");
                }
            }
        }
    }
}

/// The region and the window under its center: which program, its window
/// class and DPI, because stitching depends on how that program renders.
unsafe fn describe(region: RECT) -> String {
    let center = POINT {
        x: (region.left + region.right) / 2,
        y: (region.top + region.bottom) / 2,
    };
    let mut text = format!(
        "region left={} top={} width={} height={}",
        region.left,
        region.top,
        region.right - region.left,
        region.bottom - region.top
    );
    // The selection overlay still covers the region; skip our own windows.
    let own = GetCurrentProcessId();
    let mut window = GetTopWindow(null_mut());
    let mut process_id = 0;
    while !window.is_null() {
        let mut rect = RECT::default();
        GetWindowThreadProcessId(window, &mut process_id);
        if process_id != own
            && IsWindowVisible(window) != 0
            && GetWindowRect(window, &mut rect) != 0
            && contains(rect, center)
        {
            break;
        }
        window = GetWindow(window, GW_HWNDNEXT);
    }
    if window.is_null() {
        return text;
    }
    let mut local = center;
    ScreenToClient(window, &mut local);
    let found = ChildWindowFromPointEx(window, local, CWP_SKIPINVISIBLE);
    let child = if found.is_null() { window } else { found };
    let mut class = [0u16; 256];
    let class_len = GetClassNameW(child, class.as_mut_ptr(), class.len() as i32).max(0);
    let mut title = [0u16; 256];
    let title_len = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32).max(0);
    let _ = write!(
        text,
        "\nwindow process={} class={} title={} dpi={}",
        process_path(process_id),
        String::from_utf16_lossy(&class[..class_len as usize]),
        String::from_utf16_lossy(&title[..title_len as usize]),
        GetDpiForWindow(window)
    );
    text
}

unsafe fn process_path(process_id: u32) -> String {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
    if process.is_null() {
        return format!("pid {process_id}");
    }
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len);
    CloseHandle(process);
    if ok == 0 {
        return format!("pid {process_id}");
    }
    String::from_utf16_lossy(&path[..len as usize])
}
