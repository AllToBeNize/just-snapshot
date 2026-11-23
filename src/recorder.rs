//! Screen recording to MP4 on a background thread: H.264 video of a screen
//! area through Media Foundation (hardware encoders when present) and AAC
//! audio mixed from the system output (loopback) and the microphone.
//!
//! Video and audio share one clock, the time since recording started minus
//! the time spent paused. Video gets a constant frame rate (late frames are
//! dropped, never bunched); audio sources are pulled every frame and written
//! in step with that clock, with silence where a source has nothing (loopback
//! sends nothing while the system is quiet).

use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{Interface, GUID, HSTRING},
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
                IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
            },
        },
        Media::{
            Audio::{
                eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
                MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE,
            },
            MediaFoundation::*,
        },
        System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED},
    },
};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{
        DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, ICONINFO,
        SM_CXCURSOR,
    },
};

use crate::capture::Surface;

const SAMPLE_RATE: u32 = 48_000;
/// 100-ns units per second, Media Foundation's time base.
const TICKS: i64 = 10_000_000;
/// The largest frame H.264 encoders take everywhere; bigger areas are scaled down.
const MAX_SIZE: (i32, i32) = (3840, 2160);
/// Audio a source may run ahead before the oldest part is dropped.
const MAX_LEAD: usize = SAMPLE_RATE as usize / 5;

/// How large the video is: the area as it is on screen, or scaled down to fit 1080p or 720p.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Original,
    P1080,
    P720,
}

impl Quality {
    pub const ALL: [Self; 3] = [Self::Original, Self::P1080, Self::P720];

    pub fn name(self) -> &'static str {
        match self {
            Self::Original => "原画",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }

    /// The largest frame, landscape; portrait areas get the same box turned.
    fn limit(self) -> (i32, i32) {
        match self {
            Self::Original => MAX_SIZE,
            Self::P1080 => (1920, 1080),
            Self::P720 => (1280, 720),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub fps: u32,
    pub quality: Quality,
    pub system_audio: bool,
    pub microphone: bool,
    pub cursor: bool,
    /// Count 3, 2, 1 before an area recording starts. Not used by the recorder itself.
    pub countdown: bool,
    /// Show the key / gamepad panel inside the area while recording it. Not used by the recorder itself.
    pub show_keys: bool,
    pub show_pad: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: 60,
            quality: Quality::Original,
            system_audio: true,
            microphone: false,
            cursor: true,
            countdown: false,
            show_keys: false,
            show_pad: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Pause,
    Resume,
    Stop,
    Cancel,
}

/// What a finished recording produced.
#[derive(Debug)]
pub struct Summary {
    pub path: PathBuf,
    pub seconds: f64,
    pub frames: u64,
    pub dropped: u64,
    /// Sources that could not be opened, said plainly; the recording went on without them.
    pub warnings: Vec<String>,
    pub cancelled: bool,
}

pub struct Recording {
    commands: Sender<Command>,
    worker: Option<JoinHandle<Result<Summary, String>>>,
}

impl Recording {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Waits for the file to be finished. Call after `Stop` or `Cancel`.
    pub fn finish(mut self) -> Result<Summary, String> {
        match self.worker.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            _ => Err("录像线程意外退出，文件可能不完整。".into()),
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.commands.send(Command::Cancel);
            let _ = worker.join();
        }
    }
}

/// Starts recording `area` (screen coordinates) into `path`. Errors that
/// happen before the first frame are returned here; later ones from `finish`.
pub fn start(area: RECT, path: &Path, options: Options) -> Result<Recording, String> {
    let (commands, inbox) = mpsc::channel();
    let (ready, started) = mpsc::channel();
    let path = path.to_path_buf();
    let worker = thread::Builder::new()
        .name("snapshot-recorder".into())
        .spawn(move || unsafe { record(area, &path, options, &inbox, &ready) })
        .map_err(|error| format!("无法开始录像。\n{error}"))?;
    match started.recv() {
        Ok(Ok(())) => Ok(Recording { commands, worker: Some(worker) }),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => Err(worker.join().ok().and_then(Result::err).unwrap_or_else(|| "无法开始录像。".into())),
    }
}

/// The encoded size: the area, scaled to fit the quality's box, with even sides.
fn video_size(width: i32, height: i32, quality: Quality) -> (i32, i32) {
    let (long, short) = quality.limit();
    let (box_width, box_height) = if width >= height { (long, short) } else { (short, long) };
    let scale = (box_width as f64 / width as f64).min(box_height as f64 / height as f64).min(1.0);
    let even = |value: f64| ((value.round() as i32) & !1).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

fn mf(context: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |error| format!("{context}\n{}", error.message())
}

unsafe fn video_type(subtype: GUID, width: i32, height: i32, fps: u32) -> windows::core::Result<IMFMediaType> {
    let kind = MFCreateMediaType()?;
    kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    kind.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
    kind.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    kind.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
    kind.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)?;
    kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
    Ok(kind)
}

unsafe fn audio_type(subtype: GUID) -> windows::core::Result<IMFMediaType> {
    let kind = MFCreateMediaType()?;
    kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    kind.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
    kind.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    kind.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
    kind.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
    if subtype == MFAudioFormat_AAC {
        // 192 kbit/s.
        kind.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
    } else {
        kind.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        kind.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * 4)?;
    }
    Ok(kind)
}

unsafe fn write(writer: &IMFSinkWriter, stream: u32, bytes: &[u8], time: i64, duration: i64) -> windows::core::Result<()> {
    let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
    let mut data = null_mut();
    buffer.Lock(&mut data, None, None)?;
    data.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
    buffer.Unlock()?;
    buffer.SetCurrentLength(bytes.len() as u32)?;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buffer)?;
    sample.SetSampleTime(time)?;
    sample.SetSampleDuration(duration)?;
    writer.WriteSample(stream, &sample)
}

/// One audio device being captured, converted to 48 kHz stereo.
struct Source {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    rate: u32,
    channels: usize,
    float: bool,
    /// Where the resampler stands between input frames.
    phase: f64,
    last: [f32; 2],
    pending: VecDeque<[f32; 2]>,
}

const FLOAT_FORMAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

impl Source {
    unsafe fn open(loopback: bool) -> Result<Self, String> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|error| error.message())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(if loopback { eRender } else { eCapture }, eConsole)
            .map_err(|_| if loopback { "没有找到播放设备".to_string() } else { "没有找到麦克风".to_string() })?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|error| error.message())?;
        let format = client.GetMixFormat().map_err(|error| error.message())?;
        let (rate, channels, bits, tag) = {
            let header: &WAVEFORMATEX = &*format;
            (header.nSamplesPerSec, usize::from(header.nChannels), header.wBitsPerSample, header.wFormatTag)
        };
        let float = match tag {
            3 => true,
            0xfffe => {
                let extensible = &*(format as *const WAVEFORMATEXTENSIBLE);
                let sub = extensible.SubFormat;
                sub == FLOAT_FORMAT
            }
            _ => false,
        };
        let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        let opened = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, TICKS, 0, format, None);
        CoTaskMemFree(Some(format as _));
        opened.map_err(|error| error.message())?;
        if !(float && bits == 32 || !float && bits == 16) || channels == 0 {
            return Err(format!("不支持的音频格式（{bits} 位）"));
        }
        let capture: IAudioCaptureClient = client.GetService().map_err(|error| error.message())?;
        client.Start().map_err(|error| error.message())?;
        Ok(Self { client, capture, rate, channels, float, phase: 0.0, last: [0.0; 2], pending: VecDeque::new() })
    }

    /// Moves everything the device has into `pending`, as 48 kHz stereo.
    unsafe fn pull(&mut self) {
        loop {
            let Ok(size) = self.capture.GetNextPacketSize() else { return };
            if size == 0 {
                return;
            }
            let (mut data, mut frames, mut flags) = (null_mut(), 0u32, 0u32);
            if self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                return;
            }
            let silent = flags & 0x2 != 0;
            let step = f64::from(self.rate) / f64::from(SAMPLE_RATE);
            for index in 0..frames as usize {
                let frame = if silent {
                    [0.0, 0.0]
                } else {
                    let read = |channel: usize| -> f32 {
                        let at = index * self.channels + channel.min(self.channels - 1);
                        if self.float {
                            *(data as *const f32).add(at)
                        } else {
                            f32::from(*(data as *const i16).add(at)) / 32768.0
                        }
                    };
                    [read(0), read(1)]
                };
                // Linear resampling from the device rate to 48 kHz.
                while self.phase < 1.0 {
                    let t = self.phase as f32;
                    self.pending.push_back([
                        self.last[0] + (frame[0] - self.last[0]) * t,
                        self.last[1] + (frame[1] - self.last[1]) * t,
                    ]);
                    self.phase += step;
                }
                self.phase -= 1.0;
                self.last = frame;
            }
            let _ = self.capture.ReleaseBuffer(frames);
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Shrinks the captured area on the GPU (the D3D11 video processor) before it
/// is read back, for the 1080p and 720p qualities.
struct Scaler {
    video: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    output: ID3D11VideoProcessorOutputView,
    /// The area cut out of the desktop, the processor's input.
    crop: ID3D11Texture2D,
    scaled: ID3D11Texture2D,
}

unsafe fn texture(device: &ID3D11Device, width: u32, height: u32, staging: bool) -> Option<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: if staging { 0 } else { (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32 },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut texture = None;
    device.CreateTexture2D(&desc, None, Some(&mut texture)).ok()?;
    texture
}

impl Scaler {
    unsafe fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, from: (u32, u32), to: (u32, u32)) -> Option<Self> {
        let video_device = device.cast::<ID3D11VideoDevice>().ok()?;
        let video = context.cast::<ID3D11VideoContext>().ok()?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: from.0,
            InputHeight: from.1,
            OutputFrameRate: rate,
            OutputWidth: to.0,
            OutputHeight: to.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = video_device.CreateVideoProcessorEnumerator(&content).ok()?;
        let processor = video_device.CreateVideoProcessor(&enumerator, 0).ok()?;
        let crop = texture(device, from.0, from.1, false)?;
        let scaled = texture(device, to.0, to.1, false)?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input = None;
        video_device.CreateVideoProcessorInputView(&crop, &enumerator, &input_desc, Some(&mut input)).ok()?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output = None;
        video_device.CreateVideoProcessorOutputView(&scaled, &enumerator, &output_desc, Some(&mut output)).ok()?;
        Some(Self { video, processor, input: input?, output: output?, crop, scaled })
    }

    unsafe fn run(&self) -> bool {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let done = self.video.VideoProcessorBlt(&self.processor, &self.output, 0, &streams).is_ok();
        let [stream] = streams;
        drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
        done
    }
}

/// Copies the screen through the GPU (DXGI desktop duplication): a few
/// milliseconds a frame where GDI takes twenty. Readback is double-buffered,
/// so the CPU never waits for the copy it just asked for, and scaling
/// happens on the GPU. Only for an area inside one unrotated monitor;
/// anything else, or a lost duplication, falls back to GDI.
struct Duplicator {
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    scaler: Option<Scaler>,
    /// Two readback textures at the video size, used in turn.
    staging: [ID3D11Texture2D; 2],
    /// The slot holding the newest copy.
    latest: Option<usize>,
    /// The area's corner inside its monitor, and its size.
    origin: (u32, u32),
    area: (u32, u32),
    size: (u32, u32),
}

impl Duplicator {
    unsafe fn new(area: RECT, width: i32, height: i32) -> Option<Self> {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .ok()?;
        let (device, context) = (device?, context?);
        let adapter = device.cast::<IDXGIDevice>().ok()?.GetAdapter().ok()?;
        let (output, bounds) = (0..)
            .map_while(|index| adapter.EnumOutputs(index).ok())
            .find_map(|output| {
                let desc = output.GetDesc().ok()?;
                let bounds = desc.DesktopCoordinates;
                let inside = area.left >= bounds.left && area.top >= bounds.top && area.right <= bounds.right && area.bottom <= bounds.bottom;
                (inside && desc.Rotation.0 <= 1).then_some((output, bounds))
            })?;
        let duplication = output.cast::<IDXGIOutput1>().ok()?.DuplicateOutput(&device).ok()?;
        let area_size = ((area.right - area.left) as u32, (area.bottom - area.top) as u32);
        let size = (width as u32, height as u32);
        let scaler = if area_size == size { None } else { Some(Scaler::new(&device, &context, area_size, size)?) };
        Some(Self {
            context,
            duplication,
            scaler,
            staging: [texture(&device, size.0, size.1, true)?, texture(&device, size.0, size.1, true)?],
            latest: None,
            origin: ((area.left - bounds.left) as u32, (area.top - bounds.top) as u32),
            area: area_size,
            size,
        })
    }

    /// Copies the newest finished frame into `target` (a bitmap the video's
    /// size). False when duplication stopped working (a desktop switch, a mode change).
    unsafe fn grab_into(&mut self, target: &Surface) -> bool {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // The first frame may take a moment; after that an unchanged screen reuses the last copy.
        let wait = if self.latest.is_some() { 0 } else { 100 };
        let read = match self.duplication.AcquireNextFrame(wait, &mut info, &mut resource) {
            Ok(()) => {
                let write = self.latest.map_or(0, |latest| 1 - latest);
                let region = D3D11_BOX {
                    left: self.origin.0,
                    top: self.origin.1,
                    front: 0,
                    right: self.origin.0 + self.area.0,
                    bottom: self.origin.1 + self.area.1,
                    back: 1,
                };
                let copied = resource.and_then(|resource| resource.cast::<ID3D11Texture2D>().ok()).is_some_and(|desktop| {
                    match &self.scaler {
                        Some(scaler) => {
                            self.context.CopySubresourceRegion(&scaler.crop, 0, 0, 0, 0, &desktop, 0, Some(&region));
                            scaler.run() && {
                                self.context.CopyResource(&self.staging[write], &scaler.scaled);
                                true
                            }
                        }
                        None => {
                            self.context.CopySubresourceRegion(&self.staging[write], 0, 0, 0, 0, &desktop, 0, Some(&region));
                            true
                        }
                    }
                });
                let _ = self.duplication.ReleaseFrame();
                if !copied {
                    return false;
                }
                // Read the copy made a frame ago; it is finished, so mapping does not stall.
                let read = self.latest.unwrap_or(write);
                self.latest = Some(write);
                read
            }
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => match self.latest {
                Some(latest) => latest,
                None => return false,
            },
            Err(_) => return false,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if self.context.Map(&self.staging[read], 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
            return false;
        }
        target.write_rows(mapped.pData as *const u8, mapped.RowPitch as usize, self.size.0 as usize, self.size.1 as usize);
        self.context.Unmap(&self.staging[read], 0);
        true
    }
}

/// The screen area copied into a bitmap the size of the video.
struct Grabber {
    area: RECT,
    /// The video frame.
    frame: Surface,
    /// The area at its own size, for the GDI fallback when the video is scaled.
    unscaled: Option<Surface>,
    width: i32,
    height: i32,
    cursor: bool,
    duplicator: Option<Duplicator>,
    /// When duplication last failed, so it is retried now and then rather than every frame.
    retry: Option<Instant>,
}

impl Grabber {
    unsafe fn new(area: RECT, width: i32, height: i32, cursor: bool) -> io::Result<Self> {
        let (area_width, area_height) = (area.right - area.left, area.bottom - area.top);
        let unscaled = if (area_width, area_height) != (width, height) { Some(Surface::new(null_mut(), area_width, area_height)?) } else { None };
        Ok(Self {
            area,
            frame: Surface::new(null_mut(), width, height)?,
            unscaled,
            width,
            height,
            cursor,
            duplicator: if interactive() { Duplicator::new(area, width, height) } else { None },
            retry: None,
        })
    }

    unsafe fn grab(&mut self) -> &[u8] {
        let duplicated = match &mut self.duplicator {
            Some(duplicator) => duplicator.grab_into(&self.frame),
            None => false,
        };
        if !duplicated {
            if self.duplicator.take().is_some() || self.retry.is_none() {
                self.retry = Some(Instant::now());
            }
            if self.retry.is_some_and(|since| since.elapsed() > Duration::from_secs(1)) && interactive() {
                self.duplicator = Duplicator::new(self.area, self.width, self.height);
                self.retry = Some(Instant::now());
            }
            self.grab_gdi();
        }
        if self.cursor {
            self.draw_cursor();
        }
        GdiFlush();
        self.frame.bytes(self.width, self.height)
    }

    unsafe fn grab_gdi(&self) {
        let (area_width, area_height) = (self.area.right - self.area.left, self.area.bottom - self.area.top);
        let screen = GetDC(null_mut());
        match &self.unscaled {
            Some(unscaled) => {
                BitBlt(unscaled.dc, 0, 0, area_width, area_height, screen, self.area.left, self.area.top, SRCCOPY | CAPTUREBLT);
                SetStretchBltMode(self.frame.dc, HALFTONE);
                StretchBlt(self.frame.dc, 0, 0, self.width, self.height, unscaled.dc, 0, 0, area_width, area_height, SRCCOPY);
            }
            None => {
                BitBlt(self.frame.dc, 0, 0, area_width, area_height, screen, self.area.left, self.area.top, SRCCOPY | CAPTUREBLT);
            }
        }
        ReleaseDC(null_mut(), screen);
    }

    unsafe fn draw_cursor(&self) {
        let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut info) == 0 || info.flags & CURSOR_SHOWING == 0 {
            return;
        }
        let mut icon = ICONINFO::default();
        if GetIconInfo(info.hCursor, &mut icon) == 0 {
            return;
        }
        // Scaled with the video, so it keeps its size relative to the screen.
        let scale = self.width as f64 / f64::from(self.area.right - self.area.left);
        let place = |value: i32| (f64::from(value) * scale).round() as i32;
        let x = place(info.ptScreenPos.x - self.area.left - icon.xHotspot as i32);
        let y = place(info.ptScreenPos.y - self.area.top - icon.yHotspot as i32);
        let size = place(GetSystemMetrics(SM_CXCURSOR)).max(8);
        DrawIconEx(self.frame.dc, x, y, info.hCursor, size, size, 0, null_mut(), DI_NORMAL);
        for bitmap in [icon.hbmColor, icon.hbmMask] {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
        }
    }
}

use std::mem::size_of;

/// False in session 0, where services run without a display.
unsafe fn interactive() -> bool {
    use windows_sys::Win32::System::{RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId};
    let mut session = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &mut session) != 0 && session != 0
}

unsafe fn record(
    area: RECT,
    path: &Path,
    options: Options,
    inbox: &Receiver<Command>,
    ready: &Sender<Result<(), String>>,
) -> Result<Summary, String> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let started = MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(mf("无法启动 Windows 媒体组件。"));
    if let Err(error) = started {
        let _ = ready.send(Err(error.clone()));
        CoUninitialize();
        return Err(error);
    }
    let result = record_started(area, path, options, inbox, ready);
    let _ = MFShutdown();
    CoUninitialize();
    result
}

unsafe fn record_started(
    area: RECT,
    path: &Path,
    options: Options,
    inbox: &Receiver<Command>,
    ready: &Sender<Result<(), String>>,
) -> Result<Summary, String> {
    let fail = |error: String| {
        let _ = ready.send(Err(error.clone()));
        error
    };
    let (area_width, area_height) = (area.right - area.left, area.bottom - area.top);
    if area_width < 2 || area_height < 2 {
        return Err(fail("录像区域太小。".into()));
    }
    let (width, height) = video_size(area_width, area_height, options.quality);
    let fps = options.fps.clamp(10, 60);
    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    for (wanted, loopback, name) in [(options.system_audio, true, "系统声音"), (options.microphone, false, "麦克风")] {
        if wanted {
            match Source::open(loopback) {
                Ok(source) => sources.push(source),
                Err(error) => warnings.push(format!("{name}没有录进去：{error}")),
            }
        }
    }
    let setup = |hardware: bool| -> windows::core::Result<(IMFSinkWriter, u32, Option<u32>)> {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.unwrap();
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, u32::from(hardware))?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attributes)?;
        let output = video_type(MFVideoFormat_H264, width, height, fps)?;
        // About 0.12 bits per pixel per frame: sharp text without huge files.
        let bitrate = (f64::from(width * height) * f64::from(fps) * 0.12).clamp(2.0e6, 40.0e6) as u32;
        output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        let video = writer.AddStream(&output)?;
        let input = video_type(MFVideoFormat_RGB32, width, height, fps)?;
        // Positive stride: rows run top to bottom, as in the capture bitmap.
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, (width * 4) as u32)?;
        writer.SetInputMediaType(video, &input, None)?;
        let audio = if sources.is_empty() {
            None
        } else {
            let stream = writer.AddStream(&audio_type(MFAudioFormat_AAC)?)?;
            writer.SetInputMediaType(stream, &audio_type(MFAudioFormat_PCM)?, None)?;
            Some(stream)
        };
        writer.BeginWriting()?;
        Ok((writer, video, audio))
    };
    // Hardware encoders first; the software one when they refuse the format.
    // Without a display (a service session) drivers can crash, so software only.
    let hardware = interactive() && std::env::var_os("SNAPSHOT_SOFTWARE_ENCODER").is_none();
    let made = match setup(hardware) {
        Ok(made) => Ok(made),
        Err(_) if hardware => {
            let _ = std::fs::remove_file(path);
            setup(false)
        }
        Err(error) => Err(error),
    };
    let (writer, video, audio) = made.map_err(|error| {
        fail(format!(
            "无法创建录像文件。\n{}\n{}",
            path.display(),
            error.message()
        ))
    })?;
    let mut grabber = Grabber::new(area, width, height, options.cursor).map_err(|error| fail(format!("无法开始录像。\n{error}")))?;
    let _ = ready.send(Ok(()));

    // Finer sleeps keep 60 fps steady.
    windows_sys::Win32::Media::timeBeginPeriod(1);
    let period = TICKS / i64::from(fps);
    let begin = Instant::now();
    let mut paused_since: Option<Instant> = None;
    let mut paused_total = Duration::ZERO;
    let (mut frames, mut dropped, mut next_frame) = (0u64, 0u64, 0i64);
    let mut written_audio = 0u64;
    let mut cancelled = false;
    let mut failure = None;
    'record: loop {
        loop {
            match inbox.try_recv() {
                Ok(Command::Pause) if paused_since.is_none() => paused_since = Some(Instant::now()),
                Ok(Command::Resume) => {
                    if let Some(since) = paused_since.take() {
                        paused_total += since.elapsed();
                    }
                }
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => break 'record,
                Ok(Command::Cancel) => {
                    cancelled = true;
                    break 'record;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
            }
        }
        for source in &mut sources {
            source.pull();
        }
        if paused_since.is_some() {
            // Paused: drop what the devices hear, keep the clock still.
            for source in &mut sources {
                source.pending.clear();
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let now = (begin.elapsed() - paused_total).as_nanos() as i64 / 100;
        if now >= next_frame {
            let index = now / period;
            if frames > 0 && index as u64 > frames {
                dropped += index as u64 - frames;
            }
            let timer = Instant::now();
            let bytes = grabber.grab();
            let grabbed = timer.elapsed();
            let written = write(&writer, video, bytes, index * period, period);
            if std::env::var_os("SNAPSHOT_PROFILE").is_some() {
                eprintln!("profile grab {:.1} ms write {:.1} ms", grabbed.as_secs_f64() * 1000.0, (timer.elapsed() - grabbed).as_secs_f64() * 1000.0);
            }
            if let Err(error) = written {
                failure = Some(format!("录像写入失败。\n{}", error.message()));
                break;
            }
            frames = index as u64 + 1;
            next_frame = (index + 1) * period;
        }
        if let Some(stream) = audio {
            let due = (now as u64 * u64::from(SAMPLE_RATE) / TICKS as u64).saturating_sub(written_audio) as usize;
            // Write in chunks of at least 10 ms.
            if due >= SAMPLE_RATE as usize / 100 {
                let mut mixed = vec![[0f32; 2]; due];
                for source in &mut sources {
                    // A source far ahead of the clock (a drifting device) loses its oldest audio.
                    let excess = source.pending.len().saturating_sub(due + MAX_LEAD);
                    source.pending.drain(..excess);
                    for (slot, frame) in mixed.iter_mut().zip(source.pending.drain(..due.min(source.pending.len()))) {
                        slot[0] += frame[0];
                        slot[1] += frame[1];
                    }
                }
                let bytes: Vec<u8> = mixed
                    .iter()
                    .flat_map(|frame| frame.iter().map(|value| (value.clamp(-1.0, 1.0) * 32767.0) as i16))
                    .flat_map(i16::to_le_bytes)
                    .collect();
                let time = written_audio as i64 * TICKS / i64::from(SAMPLE_RATE);
                let duration = due as i64 * TICKS / i64::from(SAMPLE_RATE);
                if let Err(error) = write(&writer, stream, &bytes, time, duration) {
                    failure = Some(format!("录音写入失败。\n{}", error.message()));
                    break;
                }
                written_audio += due as u64;
            }
        }
        let wait = (next_frame - (begin.elapsed() - paused_total).as_nanos() as i64 / 100).clamp(0, 50_000) / 10_000;
        thread::sleep(Duration::from_millis(wait.max(1) as u64));
    }
    windows_sys::Win32::Media::timeEndPeriod(1);
    drop(sources);
    let finalized = writer.Finalize();
    let seconds = frames as f64 / f64::from(fps);
    if cancelled {
        drop(writer);
        let _ = std::fs::remove_file(path);
        return Ok(Summary { path: path.to_path_buf(), seconds, frames, dropped, warnings, cancelled: true });
    }
    if let Some(error) = failure {
        return Err(error);
    }
    finalized.map_err(mf("无法完成录像文件。"))?;
    Ok(Summary { path: path.to_path_buf(), seconds, frames, dropped, warnings, cancelled: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_sizes_are_even_and_fit_the_encoder() {
        assert_eq!(video_size(1921, 1081, Quality::Original), (1920, 1080));
        assert_eq!(video_size(4480, 1440, Quality::Original), (3840, 1234));
        assert_eq!(video_size(3, 3, Quality::Original), (2, 2));
        assert_eq!(video_size(2560, 1440, Quality::P1080), (1920, 1080));
        assert_eq!(video_size(2560, 1440, Quality::P720), (1280, 720));
        assert_eq!(video_size(1080, 1920, Quality::P720), (720, 1280), "portrait keeps its shape");
        assert_eq!(video_size(800, 600, Quality::P1080), (800, 600), "never scaled up");
    }

    /// Three seconds of a full-HD area at the default 60 fps; prints dropped
    /// frames. Run with `<test binary> full_hd_at_60 --ignored --nocapture`.
    #[test]
    #[ignore]
    fn full_hd_at_60() {
        let path = std::env::temp_dir().join(format!("snapshot-record-60-{}.mp4", std::process::id()));
        // SNAPSHOT_TEST_SIZE=2560x1440 and SNAPSHOT_TEST_QUALITY=1080p try other cases.
        let size = std::env::var("SNAPSHOT_TEST_SIZE").ok().and_then(|text| {
            let (width, height) = text.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        });
        let (width, height) = size.unwrap_or((1920, 1080));
        let quality = std::env::var("SNAPSHOT_TEST_QUALITY")
            .ok()
            .and_then(|key| Quality::ALL.into_iter().find(|quality| quality.key() == key))
            .unwrap_or_default();
        let options = Options { quality, ..Options::default() };
        let recording = start(RECT { left: 0, top: 0, right: width, bottom: height }, &path, options).unwrap();
        thread::sleep(Duration::from_secs(3));
        recording.send(Command::Stop);
        let summary = recording.finish().unwrap();
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        println!("frames {} dropped {} seconds {:.2} size {} KB warnings {:?}", summary.frames, summary.dropped, summary.seconds, size / 1024, summary.warnings);
        assert!(summary.dropped * 20 <= summary.frames, "keeps up with 60 fps");
        let _ = std::fs::remove_file(&path);
    }

    /// Records a second of the screen and reads the file back. Run with
    /// `<test binary> recorder --ignored --nocapture`.
    #[test]
    #[ignore]
    fn records_a_playable_mp4() {
        unsafe {
            let path = std::env::temp_dir().join(format!("snapshot-record-test-{}.mp4", std::process::id()));
            let options = Options { fps: 30, system_audio: true, ..Options::default() };
            let recording = start(RECT { left: 0, top: 0, right: 320, bottom: 240 }, &path, options).unwrap();
            thread::sleep(Duration::from_millis(600));
            recording.send(Command::Pause);
            thread::sleep(Duration::from_millis(400));
            recording.send(Command::Resume);
            thread::sleep(Duration::from_millis(600));
            recording.send(Command::Stop);
            let summary = recording.finish().unwrap();
            println!("{summary:?}");
            assert!(!summary.cancelled);
            assert!((1.0..1.5).contains(&summary.seconds), "pause is left out: {}", summary.seconds);
            let size = std::fs::metadata(&path).unwrap().len();
            assert!(size > 1000, "file has data: {size}");
            // Media Foundation reads it back as 320×240 video.
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).unwrap();
            let kind = reader.GetCurrentMediaType(0xFFFF_FFFC).unwrap();
            let frame = kind.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
            assert_eq!((frame >> 32, frame & 0xffff_ffff), (320, 240));
            drop(reader);
            let _ = MFShutdown();
            std::fs::remove_file(&path).unwrap();
        }
    }
}
