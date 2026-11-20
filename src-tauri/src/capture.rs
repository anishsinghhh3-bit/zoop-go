mod timing;

use crate::process as child_process;
use std::{
    cell::RefCell,
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    os::windows::process::CommandExt,
    path::Path,
    process::{self, Child, Command, Stdio},
    ptr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};
use timing::FrameTimeline;

use serde::{Deserialize, Serialize};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, EnumDisplayDevicesW, GetMonitorInfoW,
            MONITORINFO,
        },
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON},
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GA_ROOT, GetAncestor,
            GetPhysicalCursorPos, GetShellWindow, GetWindowRect, HC_ACTION, HHOOK, IsIconic,
            MONITORINFOF_PRIMARY, MSG, MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
            WindowFromPoint,
        },
    },
};

const MIN_CAPTURE_SIZE: u32 = 16;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const ENCODER_PROGRESS_TIMEOUT: Duration = Duration::from_secs(15);
const ENCODER_FINISH_TIMEOUT: Duration = Duration::from_secs(12);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    Display,
    Window,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub kind: SourceKind,
    pub id: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Selection {
    Display {
        id: String,
    },
    Window {
        id: String,
    },
    Region {
        id: String,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MouseSample {
    pub time: f64,
    pub x: f64,
    pub y: f64,
    pub click: bool,
}

#[derive(Debug, Serialize)]
pub struct CaptureResult {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub mouse: Vec<MouseSample>,
}

struct DpiScope(DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn physical() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

fn display_identity(monitor: &Monitor, rect: RECT) -> String {
    let alias = monitor.device_name().ok();
    let mut paths = Vec::new();
    if let Some(alias) = &alias {
        let name: Vec<u16> = alias.encode_utf16().chain([0]).collect();
        for index in 0..=u32::MAX {
            let mut device = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if unsafe {
                EnumDisplayDevicesW(
                    name.as_ptr(),
                    index,
                    &mut device,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                )
            } == 0
            {
                break;
            }
            let length = device
                .DeviceID
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(device.DeviceID.len());
            if device.StateFlags & DISPLAY_DEVICE_ACTIVE != 0 && length != 0 {
                paths.push(
                    String::from_utf16_lossy(&device.DeviceID[..length]).to_ascii_lowercase(),
                );
            }
        }
    }
    if paths.is_empty() {
        // Virtual displays can omit a device path. Keep these IDs scoped to the running app.
        let started = process_start_time(process::id()).unwrap_or_default();
        return format!(
            "virtual:{started:x}:{:p}:{}:{}:{}:{}:{}",
            monitor.as_raw_hmonitor(),
            alias.as_deref().unwrap_or(""),
            rect.left,
            rect.top,
            rect.right,
            rect.bottom
        );
    }

    paths.sort_unstable();
    paths.dedup();
    let mut fingerprint = DefaultHasher::new();
    paths.hash(&mut fingerprint);
    format!(
        "display:{}:{:x}",
        alias.as_deref().unwrap_or(""),
        fingerprint.finish()
    )
}

fn display_sources() -> Result<Vec<(Source, Monitor)>, String> {
    let monitors = Monitor::enumerate().map_err(|err| format!("Could not list displays. {err}"))?;
    let mut sources = Vec::with_capacity(monitors.len());
    for (index, monitor) in monitors.into_iter().enumerate() {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor.as_raw_hmonitor(), &mut info) } == 0 {
            continue;
        }
        let rect = info.rcMonitor;
        let source = Source {
            kind: SourceKind::Display,
            id: display_identity(&monitor, rect),
            name: monitor
                .name()
                .ok()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| format!("Display {}", index + 1)),
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        };
        sources.push((source, monitor));
    }
    sources.sort_by_key(|(source, _)| (!source.primary, source.x, source.y));
    Ok(sources)
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    let physical = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            ptr::from_mut(&mut rect).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    } >= 0;
    let found = physical || unsafe { GetWindowRect(hwnd, &mut rect) } != 0;
    (found && rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

fn process_start_time(process_id: u32) -> Option<u64> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle.is_null() {
        return None;
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let found = unsafe {
        let found = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        found
    };
    found.then_some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn window_sources() -> Result<Vec<(Source, Window)>, String> {
    let windows = Window::enumerate().map_err(|err| format!("Could not list windows. {err}"))?;
    let shell = unsafe { GetShellWindow() };
    let mut sources = Vec::new();

    for window in windows {
        let hwnd = window.as_raw_hwnd();
        if hwnd == shell || unsafe { IsIconic(hwnd) } != 0 {
            continue;
        }
        let Ok(pid) = window.process_id() else {
            continue;
        };
        if pid == process::id() {
            continue;
        }
        let mut cloaked = 0u32;
        let cloak_status = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                ptr::from_mut(&mut cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if cloak_status >= 0 && cloaked != 0 {
            continue;
        }
        let Ok(title) = window.title() else { continue };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        let Some(rect) = window_rect(hwnd) else {
            continue;
        };
        if rect.right - rect.left < MIN_CAPTURE_SIZE as i32
            || rect.bottom - rect.top < MIN_CAPTURE_SIZE as i32
        {
            continue;
        }
        let Some(created) = process_start_time(pid) else {
            continue;
        };
        sources.push((
            Source {
                kind: SourceKind::Window,
                id: format!("{pid}:{created:x}:{:x}", hwnd as usize),
                name: title.to_owned(),
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
                primary: false,
            },
            window,
        ));
    }
    Ok(sources)
}

pub fn list_sources() -> Result<Vec<Source>, String> {
    let _dpi = DpiScope::physical();
    let mut sources: Vec<Source> = display_sources()?
        .into_iter()
        .map(|(source, _)| source)
        .collect();
    sources.extend(window_sources()?.into_iter().map(|(source, _)| source));
    Ok(sources)
}

#[derive(Clone, Copy)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

enum Item {
    Display(Monitor),
    Window(Window),
}

impl TryFrom<Item> for GraphicsCaptureItemType {
    type Error = String;

    fn try_from(item: Item) -> Result<Self, Self::Error> {
        match item {
            Item::Display(monitor) => {
                TryInto::<Self>::try_into(monitor).map_err(|err| err.to_string())
            }
            Item::Window(window) => {
                TryInto::<Self>::try_into(window).map_err(|err| err.to_string())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PointerSpace {
    Display { x: i32, y: i32 },
    Window { hwnd: usize },
}

struct Target {
    item: Item,
    crop: Option<Crop>,
    pointer: PointerSpace,
}

impl Target {
    fn resolve(selection: Selection) -> Result<Self, String> {
        let (id, crop) = match selection {
            Selection::Window { id } => {
                let (_, window) = window_sources()?
                    .into_iter()
                    .find(|(source, _)| source.id == id)
                    .ok_or("That window is no longer available. Bring it into view and choose it again.")?;
                return Ok(Self {
                    pointer: PointerSpace::Window {
                        hwnd: window.as_raw_hwnd() as usize,
                    },
                    item: Item::Window(window),
                    crop: None,
                });
            }
            Selection::Display { id } => (id, None),
            Selection::Region {
                id,
                x,
                y,
                width,
                height,
            } => (
                id,
                Some(Crop {
                    x,
                    y,
                    width,
                    height,
                }),
            ),
        };
        let (source, monitor) = display_sources()?
            .into_iter()
            .find(|(source, _)| source.id == id)
            .ok_or("That display is no longer available. Choose a connected display.")?;
        if let Some(area) = crop {
            if area.width < MIN_CAPTURE_SIZE
                || area.height < MIN_CAPTURE_SIZE
                || area.x.saturating_add(area.width) > source.width
                || area.y.saturating_add(area.height) > source.height
            {
                return Err(
                    "Choose an area of at least 16 by 16 pixels within one display.".into(),
                );
            }
        }
        Ok(Self {
            item: Item::Display(monitor),
            crop,
            pointer: PointerSpace::Display {
                x: source.x + crop.map_or(0, |area| area.x as i32),
                y: source.y + crop.map_or(0, |area| area.y as i32),
            },
        })
    }
}

#[derive(Default)]
struct LatestFrame {
    pixels: Vec<u8>,
    dimensions: Option<(u32, u32)>,

    sequence: u64,
}

type FrameSlot = Arc<(Mutex<LatestFrame>, Condvar)>;

struct FrameOptions {
    latest: FrameSlot,
    crop: Option<Crop>,
    fps: u32,
}

struct FrameHandler {
    options: FrameOptions,
    scratch: Vec<u8>,
    output: Option<Crop>,
    next_read: Option<Instant>,
}

impl GraphicsCaptureApiHandler for FrameHandler {
    type Flags = FrameOptions;
    type Error = String;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            options: context.flags,
            scratch: Vec::new(),
            output: None,
            next_read: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let arrived_at = Instant::now();
        let period = Duration::from_secs_f64(1.0 / f64::from(self.options.fps));
        // An absolute schedule allows an uneven refresh cadence to contribute frames at the target rate.
        let eligible_at = arrived_at + period / 5;
        let due = self.next_read.unwrap_or(arrived_at);
        if eligible_at < due {
            return Ok(());
        }
        let mut output = self.output.or(self.options.crop).unwrap_or(Crop {
            x: 0,
            y: 0,
            width: frame.width(),
            height: frame.height(),
        });
        output.width &= !1;
        output.height &= !1;
        if output.width < MIN_CAPTURE_SIZE || output.height < MIN_CAPTURE_SIZE {
            return Ok(());
        }
        let available_width = frame.width().saturating_sub(output.x).min(output.width);
        let available_height = frame.height().saturating_sub(output.y).min(output.height);
        if available_width == 0 || available_height == 0 {
            return Ok(());
        }
        let mut buffer = frame
            .buffer_crop(
                output.x,
                output.y,
                output.x + available_width,
                output.y + available_height,
            )
            .map_err(|err| format!("Could not read a captured frame. {err}"))?;
        let row_pitch = buffer.row_pitch() as usize;
        let raw = buffer.as_raw_buffer();
        let row_size = output.width as usize * 4;
        let active_row_size = available_width as usize * 4;
        if row_pitch < active_row_size || raw.len() / row_pitch < available_height as usize {
            return Err("Windows returned an incomplete screen frame.".into());
        }
        let frame_size = row_size
            .checked_mul(output.height as usize)
            .ok_or("The recording resolution is too large.")?;
        if self.scratch.capacity() < frame_size {
            self.scratch
                .try_reserve_exact(frame_size - self.scratch.len())
                .map_err(|_| "There is not enough memory for this recording resolution.")?;
        }
        self.scratch.resize(frame_size, 0);
        if available_width != output.width || available_height != output.height {
            self.scratch.fill(0);
        }
        for (source, dest) in raw
            .chunks_exact(row_pitch)
            .take(available_height as usize)
            .zip(self.scratch.chunks_exact_mut(row_size))
        {
            dest[..active_row_size].copy_from_slice(&source[..active_row_size]);
        }
        self.output = Some(output);
        let intervals =
            (eligible_at.duration_since(due).as_secs_f64() / period.as_secs_f64()).floor();
        self.next_read = Some(due + period.mul_f64(intervals + 1.0));
        let (mutex, wake) = &*self.options.latest;
        let mut latest = mutex
            .lock()
            .map_err(|_| "The frame receiver stopped unexpectedly.")?;
        std::mem::swap(&mut latest.pixels, &mut self.scratch);
        latest.sequence += 1;
        latest.dimensions = Some((output.width, output.height));

        wake.notify_one();
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RawClick {
    point: POINT,
}

thread_local! {
    static PENDING_CLICKS: RefCell<Vec<RawClick>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn click_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    let down = matches!(
        message as u32,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    );
    if code == HC_ACTION as i32 && down && data != 0 {
        let click = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        let _ = PENDING_CLICKS.try_with(|clicks| {
            if let Ok(mut pending) = clicks.try_borrow_mut() {
                pending.push(RawClick { point: click.pt });
            }
        });
    }
    unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
}

struct MouseHook(HHOOK);

impl MouseHook {
    fn install() -> Self {
        Self(unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(click_hook),
                GetModuleHandleW(ptr::null()),
                0,
            )
        })
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { UnhookWindowsHookEx(self.0) };
        }
    }
}

fn button_state() -> [bool; 3] {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].map(|key| unsafe { GetAsyncKeyState(i32::from(key)) < 0 })
}

fn relative_position(
    space: PointerSpace,
    point: POINT,
    width: u32,
    height: u32,
) -> Option<(f64, f64)> {
    let (left, top, width, height) = match space {
        PointerSpace::Display { x, y } => (x, y, width, height),
        PointerSpace::Window { hwnd } => {
            let hwnd = hwnd as HWND;
            let hit = unsafe { WindowFromPoint(point) };
            if hit.is_null() || unsafe { GetAncestor(hit, GA_ROOT) } != hwnd {
                return None;
            }
            let rect = window_rect(hwnd)?;
            (
                rect.left,
                rect.top,
                width.min((rect.right - rect.left) as u32),
                height.min((rect.bottom - rect.top) as u32),
            )
        }
    };
    let dx = i64::from(point.x) - i64::from(left);
    let dy = i64::from(point.y) - i64::from(top);
    (dx >= 0 && dx < i64::from(width) && dy >= 0 && dy < i64::from(height))
        .then_some((dx as f64, dy as f64))
}

fn track_mouse(
    stop: &AtomicBool,
    space: PointerSpace,
    dimensions: (u32, u32),
    timeline: &FrameTimeline,
) -> Vec<MouseSample> {
    let _dpi = DpiScope::physical();
    let mouse_hook = MouseHook::install();
    let (width, height) = dimensions;
    let mut samples = Vec::new();
    let mut previous_position = None;
    let mut previous_buttons = button_state();
    let mut next_position = Instant::now();
    let mut stopped = false;

    let mut record = |point, click| {
        if click && matches!(space, PointerSpace::Display { .. }) {
            let hit = unsafe { WindowFromPoint(point) };
            if !hit.is_null() {
                let mut process_id = 0;
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                        hit,
                        &mut process_id,
                    )
                };
                if process_id == std::process::id() {
                    return;
                }
            }
        }
        let Some((x, y)) = relative_position(space, point, width, height) else {
            return;
        };
        let position = (x, y);
        if click || previous_position != Some(position) {
            samples.push(MouseSample {
                time: timeline.next_sample_time(),
                x: x / f64::from(width),
                y: y / f64::from(height),
                click,
            });
        }
        previous_position = Some(position);
    };

    while !stopped {
        stopped = stop.load(Ordering::Relaxed);
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe { DispatchMessageW(&message) };
        }
        PENDING_CLICKS.with(|pending| {
            for click in pending.borrow_mut().drain(..) {
                record(click.point, true);
            }
        });
        let now = Instant::now();
        if mouse_hook.0.is_null() {
            let buttons = button_state();
            if buttons
                .iter()
                .zip(previous_buttons)
                .any(|(down, was)| *down && !was)
            {
                let mut point = POINT::default();
                if unsafe { GetPhysicalCursorPos(&mut point) } != 0 {
                    record(point, true);
                }
            }
            previous_buttons = buttons;
        }
        if now >= next_position {
            let mut point = POINT::default();
            if unsafe { GetPhysicalCursorPos(&mut point) } != 0 {
                record(point, false);
            }
            next_position = now + Duration::from_millis(32);
        }
        if !stopped {
            thread::sleep(Duration::from_millis(3));
        }
    }
    samples
}

fn spawn_encoder(
    ffmpeg: &Path,
    destination: &Path,
    dimensions: (u32, u32),
    fps: u32,
) -> Result<Child, String> {
    let (width, height) = dimensions;
    Command::new(ffmpeg)
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &format!("{width}x{height}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "libx264",
            "-threads:v",
            "4",
            "-preset",
            "ultrafast",
            "-crf",
            "14",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mp4",
        ])
        .arg(destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start the recording encoder. {err}"))
}

fn stream_frames(
    ffmpeg: &Path,
    destination: &Path,
    latest: &FrameSlot,
    capture: &CaptureControl<FrameHandler, String>,
    stop: &AtomicBool,
    options: RecordingOptions,
    ready: &mut Option<Sender<Result<(), String>>>,
) -> Result<CaptureResult, String> {
    let RecordingOptions { fps, pointer } = options;
    let (mutex, wake) = &**latest;
    let first_deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let (dimensions, mut sequence, mut pixels) = {
        let mut state = mutex
            .lock()
            .map_err(|_| "The frame receiver stopped unexpectedly.")?;
        while state.dimensions.is_none() {
            if capture.is_finished()
                || stop.load(Ordering::Relaxed)
                || Instant::now() >= first_deadline
            {
                return Err(
                    "No frames arrived. Make sure the source is visible and try again.".into(),
                );
            }
            (state, _) = wake
                .wait_timeout(state, Duration::from_millis(30))
                .map_err(|_| "The frame receiver stopped unexpectedly.")?;
        }
        let dimensions = state.dimensions.ok_or("The source has no usable size.")?;
        (
            dimensions,
            state.sequence,
            std::mem::take(&mut state.pixels),
        )
    };

    let mut encoder = spawn_encoder(ffmpeg, destination, dimensions, fps)?;
    let mut input = encoder
        .stdin
        .take()
        .expect("encoder stdin was configured as piped");
    let error_output = encoder
        .stderr
        .take()
        .expect("encoder stderr was configured as piped");
    let mouse_stop = AtomicBool::new(false);
    let start = Instant::now();
    let mut frame_count = 0u64;
    let timeline = FrameTimeline::new(fps);
    let last_written = AtomicU64::new(0);
    let input_closed = AtomicBool::new(false);
    let (write_result, mouse, log) = thread::scope(|scope| {
        let logs = scope.spawn(move || child_process::read_diagnostic(error_output));
        let supervisor = scope.spawn(|| {
            let mut finish_started = None;
            child_process::wait_for_child(&mut encoder, || {
                if input_closed.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                    let finish_started = finish_started.get_or_insert_with(Instant::now);
                    if finish_started.elapsed() >= ENCODER_FINISH_TIMEOUT {
                        return Some("The recording encoder stalled while stopping.");
                    }
                } else {
                    let last_written = Duration::from_millis(last_written.load(Ordering::Relaxed));
                    if start.elapsed().saturating_sub(last_written) >= ENCODER_PROGRESS_TIMEOUT {
                        return Some("The recording encoder stopped accepting frames.");
                    }
                }
                None
            })
        });
        let points = scope.spawn(|| track_mouse(&mouse_stop, pointer, dimensions, &timeline));
        let result = (|| {
            loop {
                if stop.load(Ordering::Relaxed) || (frame_count != 0 && capture.is_finished()) {
                    break;
                }
                if frame_count != 0 {
                    let mut state = mutex
                        .lock()
                        .map_err(|_| "The frame receiver stopped unexpectedly.".to_owned())?;
                    if state.sequence != sequence {
                        std::mem::swap(&mut pixels, &mut state.pixels);
                        sequence = state.sequence;
                    }
                }
                timeline.frame_started();
                input.write_all(&pixels).map_err(|err| {
                    format!("The recording encoder stopped receiving frames. {err}")
                })?;
                frame_count += 1;
                last_written.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                if let Some(sender) = ready.take() {
                    let _ = sender.send(Ok(()));
                }
                let deadline = start + Duration::from_secs_f64(frame_count as f64 / f64::from(fps));
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            Ok::<(), String>(())
        })();
        mouse_stop.store(true, Ordering::Relaxed);
        drop(input);
        input_closed.store(true, Ordering::Relaxed);
        let status = supervisor
            .join()
            .unwrap_or_else(|_| Err("The recording encoder stopped unexpectedly.".into()));
        let mouse = points.join().unwrap_or_default();
        let log = logs.join().unwrap_or_default();
        let result = match (result, status) {
            (_, Err(err)) | (Err(err), Ok(_)) => Err(err),
            (Ok(()), Ok(exit)) if !exit.success() => {
                Err("The recording encoder could not finalize the video.".into())
            }
            _ => Ok(()),
        };
        (result, mouse, log)
    });

    if let Err(err) = write_result {
        let log = log.trim();
        return Err(if log.is_empty() {
            err
        } else {
            format!("{err} {log}")
        });
    }
    if frame_count == 0 {
        return Err("The recording ended before any frames were saved.".into());
    }
    let duration = frame_count as f64 / f64::from(fps);
    let (width, height) = dimensions;
    let mouse = mouse
        .into_iter()
        .filter(|sample| sample.time < duration)
        .collect();
    Ok(CaptureResult {
        width,
        height,
        duration,
        mouse,
    })
}

struct RecordingOptions {
    fps: u32,
    pointer: PointerSpace,
}

pub fn run_capture(
    ffmpeg: &Path,
    raw_output: &Path,
    selection: Selection,
    fps: u32,
    stop: Arc<AtomicBool>,
    ready: Sender<Result<(), String>>,
) -> Result<CaptureResult, String> {
    let _dpi = DpiScope::physical();
    let mut ready = Some(ready);
    let result = (|| {
        if !matches!(fps, 30 | 60) {
            return Err("Choose either 30 or 60 frames per second.".to_owned());
        }
        let latest = FrameSlot::default();
        let mut retried = false;
        let (capture, pointer) = loop {
            if stop.load(Ordering::Acquire) {
                return Err("Recording was stopped before Windows returned a frame.".into());
            }
            let Target {
                item,
                crop,
                pointer,
            } = Target::resolve(selection.clone())?;
            let settings = Settings::new(
                item,
                CursorCaptureSettings::Default,
                DrawBorderSettings::Default,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                FrameOptions {
                    latest: Arc::clone(&latest),
                    crop,
                    fps,
                },
            );
            match FrameHandler::start_free_threaded(settings) {
                Ok(capture) => break (capture, pointer),
                Err(windows_capture::capture::GraphicsCaptureApiError::ItemConvertFailed)
                    if !retried =>
                {
                    // DWM can briefly have no capturable surface while the source changes visibility.
                    retried = true;
                    thread::sleep(Duration::from_millis(120));
                }
                Err(windows_capture::capture::GraphicsCaptureApiError::ItemConvertFailed) => {
                    return Err(match selection {
                        Selection::Window { .. } =>
                            "Windows could not capture this window. Bring it into view, refresh the source list, and try again.",
                        _ => "Windows could not capture this display. Refresh the source list and try again.",
                    }.into());
                }
                Err(error) => {
                    return Err(format!(
                        "Windows could not start capturing this source. {error}"
                    ));
                }
            }
        };
        let result = stream_frames(
            ffmpeg,
            raw_output,
            &latest,
            &capture,
            &stop,
            RecordingOptions { fps, pointer },
            &mut ready,
        );
        let capture_stop = if capture.is_finished() {
            capture.wait()
        } else {
            capture.stop()
        };
        match (result, capture_stop) {
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(format!("Windows capture stopped unexpectedly. {err}")),
            (Ok(recording), Ok(())) => Ok(recording),
        }
    })();
    if let (Err(err), Some(sender)) = (&result, ready) {
        let _ = sender.send(Err(err.clone()));
    }
    result
}
