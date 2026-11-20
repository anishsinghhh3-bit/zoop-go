use crate::{capture, clipboard, render};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    io::{self, BufWriter, Read, Write},
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewWindow, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use windows_sys::Win32::{
    Foundation::SYSTEMTIME,
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
    System::SystemInformation::GetLocalTime,
    UI::{
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            GetWindowDisplayAffinity, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SetWindowDisplayAffinity,
            ShowWindow, WDA_EXCLUDEFROMCAPTURE,
        },
    },
};

const STATE_EVENT: &str = "recording-state";
const CONTROLS_EVENT: &str = "recording-controls-state";
const MAX_PREFERENCES_BYTES: u64 = 64 * 1024;
const FFMPEG_HASH: &[u8; 32] = include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.sha256"));

#[derive(Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Status {
    #[default]
    Idle,
    Starting,
    Recording,
    Processing,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum TargetKind {
    Monitor,
    Window,
    Region,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Target {
    kind: TargetKind,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<Region>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Format {
    Mp4,
    Gif,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Gif => "gif",
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Settings {
    target: Target,
    fps: u32,
    format: Format,
    auto_copy: bool,
}

impl Settings {
    fn selection(&self) -> Result<capture::Selection, String> {
        if !matches!(self.fps, 30 | 60) {
            return Err("Choose a frame rate of 30 or 60 fps.".into());
        }
        if self.target.id.is_empty() {
            return Err("Choose a capture source first.".into());
        }
        let id = self.target.id.clone();
        match self.target.kind {
            TargetKind::Monitor => Ok(capture::Selection::Display { id }),
            TargetKind::Window => Ok(capture::Selection::Window { id }),
            TargetKind::Region => {
                let region = self
                    .target
                    .region
                    .as_ref()
                    .ok_or("Choose an area to record.")?;
                Ok(capture::Selection::Region {
                    id,
                    x: region.x,
                    y: region.y,
                    width: region.width,
                    height: region.height,
                })
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    path: PathBuf,
    filename: String,
    format: Format,
    #[serde(default)]
    clipboard_copied: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    settings: Option<Settings>,
    last_output: Option<Output>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    status: Status,
    started_at: Option<u64>,
    last_output: Option<Output>,
    error: Option<String>,
    settings: Option<Settings>,
    shortcut_available: bool,
}

struct ActiveRecording {
    id: u64,
    stop: Arc<AtomicBool>,
    restore_main: bool,
    show_controls: bool,
    capture_ended: bool,
}

#[derive(Default)]
struct Session {
    snapshot: Snapshot,
    active: Option<ActiveRecording>,
    next_id: u64,
    settings_incomplete: bool,
}

struct Recorder {
    session: Mutex<Session>,
    preferences_path: PathBuf,
    controller_excluded: bool,
}

impl Recorder {
    fn lock(&self) -> MutexGuard<'_, Session> {
        self.session
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn load(preferences_path: PathBuf, videos: Option<&Path>, controller_excluded: bool) -> Self {
        let mut preferences: Preferences = fs::File::open(&preferences_path)
            .ok()
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(MAX_PREFERENCES_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
                    return None;
                }
                serde_json::from_slice(&bytes).ok()
            })
            .unwrap_or_default();
        preferences.last_output = preferences.last_output.filter(|output| {
            videos.is_some_and(|folder| {
                output.path.file_name() == Some(OsStr::new(&output.filename))
                    && output.path.extension() == Some(OsStr::new(output.format.extension()))
                    && is_recording_file(&output.path, folder)
            })
        });
        if let Some(output) = preferences.last_output.as_mut() {
            output.clipboard_copied = false;
        }
        Self {
            session: Mutex::new(Session {
                snapshot: Snapshot {
                    settings: preferences.settings,
                    last_output: preferences.last_output,
                    ..Snapshot::default()
                },
                // Refresh and validate the saved source before the global shortcut can record.
                settings_incomplete: true,
                ..Session::default()
            }),
            preferences_path,
            controller_excluded,
        }
    }

    fn persist(&self, snapshot: &Snapshot) -> Result<(), String> {
        let parent = self
            .preferences_path
            .parent()
            .ok_or("The settings folder is unavailable.")?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not open the settings folder. {error}"))?;
        let preferences = Preferences {
            settings: snapshot.settings.clone(),
            last_output: snapshot.last_output.clone(),
        };
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("Could not save settings. {error}"))?;
        serde_json::to_writer(file.as_file_mut(), &preferences)
            .map_err(|error| format!("Could not save settings. {error}"))?;
        file.as_file_mut()
            .flush()
            .map_err(|error| format!("Could not save settings. {error}"))?;
        file.persist(&self.preferences_path)
            .map_err(|error| format!("Could not save settings. {error}"))?;
        Ok(())
    }
}

#[derive(Serialize)]
struct TargetInfo {
    id: String,
    label: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
}

#[derive(Default, Serialize)]
struct Targets {
    monitors: Vec<TargetInfo>,
    windows: Vec<TargetInfo>,
}

#[tauri::command]
fn list_targets() -> Result<Targets, String> {
    let mut targets = Targets::default();
    for source in capture::list_sources()? {
        let is_display = matches!(source.kind, capture::SourceKind::Display);
        let target = TargetInfo {
            id: source.id,
            label: source.name,
            x: source.x,
            y: source.y,
            width: source.width,
            height: source.height,
            primary: source.primary,
        };
        if is_display {
            targets.monitors.push(target);
        } else {
            targets.windows.push(target);
        }
    }
    Ok(targets)
}

fn controls_state(snapshot: &Snapshot) -> Snapshot {
    Snapshot {
        status: snapshot.status,
        started_at: snapshot.started_at,
        shortcut_available: snapshot.shortcut_available,
        ..Snapshot::default()
    }
}

#[tauri::command]
fn app_state(window: WebviewWindow, recorder: State<'_, Recorder>) -> Snapshot {
    let session = recorder.lock();
    if window.label() == "controller" {
        controls_state(&session.snapshot)
    } else {
        session.snapshot.clone()
    }
}

#[tauri::command]
fn set_recording_settings(
    recorder: State<'_, Recorder>,
    settings: Option<Settings>,
) -> Result<(), String> {
    let mut session = recorder.lock();
    if let Some(settings) = settings {
        settings.selection()?;
        session.snapshot.settings = Some(settings);
        session.settings_incomplete = false;
        recorder.persist(&session.snapshot)?;
    } else {
        session.settings_incomplete = true;
    }
    Ok(())
}

fn emit(app: &AppHandle, session: &Session) {
    let _ = app.emit_to("main", STATE_EVENT, &session.snapshot);
    let _ = app.emit_to(
        "controller",
        CONTROLS_EVENT,
        controls_state(&session.snapshot),
    );
}

fn show_main_without_focus(app: &AppHandle) {
    if let Some(main) = app.get_webview_window("main")
        && let Ok(hwnd) = main.hwnd()
    {
        unsafe { ShowWindow(hwnd.0.cast(), SW_SHOWNOACTIVATE) };
    }
}

fn show_controller(app: &AppHandle) {
    let Some(controller) = app.get_webview_window("controller") else {
        return;
    };
    if let Ok(Some(monitor)) = controller.primary_monitor() {
        let area = monitor.work_area();
        let scale = monitor.scale_factor();
        let width = (255.0 * scale).round() as i32;
        let height = (76.0 * scale).round() as i32;
        let margin = (18.0 * scale).round() as i32;
        let _ = controller.set_position(PhysicalPosition::new(
            area.position.x + area.size.width as i32 - width - margin,
            area.position.y + area.size.height as i32 - height - margin,
        ));
    }
    if let Ok(hwnd) = controller.hwnd() {
        unsafe { ShowWindow(hwnd.0.cast(), SW_SHOWNOACTIVATE) };
    }
}

fn hide_controller(app: &AppHandle) {
    if let Some(controller) = app.get_webview_window("controller") {
        let _ = controller.hide();
    }
}

fn update_windows(app: &AppHandle, id: u64, status: Status) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let recorder = handle.state::<Recorder>();
        let session = recorder.lock();
        if session.snapshot.status != status {
            return;
        }
        let Some(active) = session.active.as_ref().filter(|active| active.id == id) else {
            return;
        };
        match status {
            Status::Recording if active.show_controls => show_controller(&handle),
            Status::Processing => {
                hide_controller(&handle);
                if active.restore_main {
                    show_main_without_focus(&handle);
                }
            }
            Status::Idle | Status::Starting | Status::Recording => {}
        }
    });
}

fn recording_folder(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .video_dir()
        .map(|folder| folder.join("Zoop Go"))
        .map_err(|_| "Windows could not locate the Videos folder.".into())
}

fn video_folder(app: &AppHandle) -> Result<PathBuf, String> {
    let folder = recording_folder(app)?;
    fs::create_dir_all(&folder)
        .map_err(|error| format!("Could not create the Zoop Go folder in Videos. {error}"))?;
    Ok(folder)
}

fn is_recording_file(path: &Path, folder: &Path) -> bool {
    path.is_absolute()
        && path.parent() == Some(folder)
        && matches!(
            path.extension().and_then(OsStr::to_str),
            Some("mp4" | "gif")
        )
        && path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("Zoop Go ") && !name.contains(':'))
        && fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        })
}

fn verify_ffmpeg(path: &Path, expected_len: u64, expected_hash: &[u8; 32]) -> io::Result<bool> {
    let entry = fs::symlink_metadata(path)?;
    if !entry.is_file()
        || entry.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || entry.len() != expected_len
    {
        return Ok(false);
    }
    let mut file = fs::File::open(path)?.take(expected_len + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().as_slice() == expected_hash)
}

fn ffmpeg_path() -> Result<PathBuf, String> {
    let executable =
        std::env::current_exe().map_err(|_| "Could not locate the recording engine.")?;
    let parent = executable
        .parent()
        .ok_or("Could not locate the recording engine.")?;
    let path = parent.join("ffmpeg.exe");
    let expected_len = env!("ZOOP_FFMPEG_BYTES")
        .parse()
        .expect("bundled FFmpeg size");
    match verify_ffmpeg(&path, expected_len, FFMPEG_HASH) {
        Ok(true) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err("The bundled FFmpeg executable is missing. Extract the complete Zoop Go package or reinstall the app.".into())
        }
        _ => Err("The FFmpeg executable could not be verified. Extract a fresh copy of Zoop Go or reinstall the app.".into()),
    }
}

fn clipboard_owner(app: &AppHandle) -> Result<isize, String> {
    app.get_webview_window("main")
        .and_then(|main| main.hwnd().ok())
        .map(|hwnd| hwnd.0 as isize)
        .ok_or("The clipboard is unavailable. Reopen Zoop Go and try Copy file again.".into())
}

fn copy_file(app: &AppHandle, path: &Path) -> Result<(), String> {
    if !is_recording_file(path, &recording_folder(app)?) {
        return Err("This recording is unavailable in the Zoop Go Videos folder.".into());
    }
    clipboard::copy_file(clipboard_owner(app)?, path)
}

fn begin(app: &AppHandle, settings: Settings, via_shortcut: bool) -> Result<(), String> {
    let selection = settings.selection()?;
    let ffmpeg = ffmpeg_path()?;
    let output_dir = video_folder(app)?;
    let workspace = tempfile::Builder::new()
        .prefix("zoop-go-")
        .tempdir()
        .map_err(|error| format!("Could not prepare temporary recording storage. {error}"))?;
    let main = app
        .get_webview_window("main")
        .ok_or("The recorder window is unavailable.")?;
    let recorder = app.state::<Recorder>();
    let restore_main = main.is_visible().unwrap_or(false) && !main.is_minimized().unwrap_or(false);
    let (id, stop) = {
        let mut session = recorder.lock();
        if session.active.is_some() {
            return Err("A recording is already in progress.".into());
        }
        if via_shortcut
            && (session.settings_incomplete
                || session.snapshot.settings.as_ref() != Some(&settings))
        {
            return Err(
                "The capture setup changed. Press Ctrl+Shift+R again when it is ready.".into(),
            );
        }
        let show_controls =
            matches!(settings.target.kind, TargetKind::Window) || recorder.controller_excluded;
        if !show_controls && !session.snapshot.shortcut_available {
            return Err("Windows could not protect the recording controls from screen capture. Use Window capture or free up Ctrl+Shift+R and reopen Zoop Go.".into());
        }
        session.next_id += 1;
        let id = session.next_id;
        let stop = Arc::new(AtomicBool::new(false));
        session.active = Some(ActiveRecording {
            id,
            stop: stop.clone(),
            restore_main,
            show_controls,
            capture_ended: false,
        });
        session.snapshot.settings = Some(settings.clone());
        session.snapshot.status = Status::Starting;
        session.snapshot.started_at = None;
        session.snapshot.error = None;
        let _ = recorder.persist(&session.snapshot);
        emit(app, &session);
        (id, stop)
    };
    if restore_main && let Err(error) = main.hide() {
        let message = format!("Could not hide the recorder before capture. {error}");
        finish(app, id, Err(message.clone()));
        return Err(message);
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let ready_app = app.clone();
    thread::spawn(move || {
        if !matches!(ready_rx.recv(), Ok(Ok(()))) {
            return;
        }
        let recorder = ready_app.state::<Recorder>();
        {
            let mut session = recorder.lock();
            if !session
                .active
                .as_ref()
                .is_some_and(|active| active.id == id)
                || session.snapshot.status != Status::Starting
            {
                return;
            }
            session.snapshot.status = Status::Recording;
            session.snapshot.started_at = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            );
            emit(&ready_app, &session);
        }
        update_windows(&ready_app, id, Status::Recording);
    });

    let worker_app = app.clone();
    thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let raw = workspace.path().join("capture.mp4");
            let captured =
                capture::run_capture(&ffmpeg, &raw, selection, settings.fps, stop, ready_tx)?;
            {
                let recorder = worker_app.state::<Recorder>();
                let mut session = recorder.lock();
                if let Some(active) = session.active.as_mut() {
                    active.capture_ended = true;
                }
                session.snapshot.status = Status::Processing;
                emit(&worker_app, &session);
            }
            update_windows(&worker_app, id, Status::Processing);

            // Rendering consumes the same samples in memory even when a scratch log cannot be written.
            let _ = write_mouse_log(
                &workspace.path().join("mouse.json"),
                &captured,
                settings.fps,
            );
            let clicks: Vec<render::Click> = captured
                .mouse
                .iter()
                .filter(|point| point.click)
                .map(|point| render::Click {
                    time: point.time,
                    x: point.x,
                    y: point.y,
                })
                .collect();
            let name = recording_name();
            let filename = format!("{name}.{}", settings.format.extension());
            let path = output_dir.join(&filename);
            let format = match settings.format {
                Format::Mp4 => render::Format::Mp4,
                Format::Gif => render::Format::Gif,
            };
            if let Err(error) = render::export(
                &ffmpeg,
                &raw,
                &path,
                captured.width,
                captured.height,
                settings.fps,
                &clicks,
                format,
                true,
            ) {
                let original_name = format!("{name}-original.mp4");
                let original_path = output_dir.join(&original_name);
                if preserve_original(&raw, &original_path).is_ok() {
                    return Ok((
                        Output {
                            path: original_path,
                            filename: original_name,
                            format: Format::Mp4,
                            clipboard_copied: false,
                        },
                        Some(format!(
                            "Export failed. The original recording was saved. {error}"
                        )),
                    ));
                }
                let recovery_folder = workspace.keep();
                return Err(format!(
                    "Export failed. The original recording is at {}. {error}",
                    recovery_folder.join("capture.mp4").display()
                ));
            }

            let clipboard_result = if settings.auto_copy {
                copy_file(&worker_app, &path)
            } else {
                Ok(())
            };
            let clipboard_copied = settings.auto_copy && clipboard_result.is_ok();
            Ok((
                Output {
                    path,
                    filename,
                    format: settings.format,
                    clipboard_copied,
                },
                clipboard_result.err(),
            ))
        }))
        .unwrap_or_else(|_| {
            Err("The recording engine stopped unexpectedly. You can start a new recording.".into())
        });
        finish(&worker_app, id, outcome);
    });

    Ok(())
}

fn write_mouse_log(path: &Path, capture: &capture::CaptureResult, fps: u32) -> io::Result<()> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MouseLog<'a> {
        duration_seconds: f64,
        width: u32,
        height: u32,
        fps: u32,
        samples: &'a [capture::MouseSample],
    }

    let mut file = BufWriter::new(fs::File::create(path)?);
    serde_json::to_writer(
        &mut file,
        &MouseLog {
            duration_seconds: capture.duration,
            width: capture.width,
            height: capture.height,
            fps,
            samples: &capture.mouse,
        },
    )?;
    file.flush()
}

fn recording_name() -> String {
    let mut time = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut time) };
    format!(
        "Zoop Go {:04}-{:02}-{:02} {:02}-{:02}-{:02}-{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

fn preserve_original(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::hard_link(source, destination).is_ok() {
        return Ok(());
    }
    let folder = destination
        .parent()
        .ok_or_else(|| io::Error::other("Missing destination directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(folder)?;
    io::copy(&mut fs::File::open(source)?, temporary.as_file_mut())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist_noclobber(destination)?;
    Ok(())
}

fn finish(app: &AppHandle, id: u64, result: Result<(Output, Option<String>), String>) {
    let recorder = app.state::<Recorder>();
    let restore_main = {
        let mut session = recorder.lock();
        let Some(active) = session.active.take_if(|active| active.id == id) else {
            return;
        };
        session.snapshot.status = Status::Idle;
        session.snapshot.started_at = None;
        match result {
            Ok((output, clipboard_error)) => {
                session.snapshot.last_output = Some(output);
                session.snapshot.error = clipboard_error;
                let _ = recorder.persist(&session.snapshot);
            }
            Err(error) => session.snapshot.error = Some(error),
        }
        emit(app, &session);
        active.restore_main || session.snapshot.error.is_some()
    };
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let recorder = handle.state::<Recorder>();
        let session = recorder.lock();
        if session.active.is_none() {
            hide_controller(&handle);
            if restore_main {
                if let Some(main) = handle.get_webview_window("main")
                    && let Ok(hwnd) = main.hwnd()
                {
                    if main.is_minimized().unwrap_or(false) {
                        unsafe {
                            ShowWindow(
                                hwnd.0.cast(),
                                windows_sys::Win32::UI::WindowsAndMessaging::SW_RESTORE,
                            )
                        };
                    } else {
                        show_main_without_focus(&handle);
                    }
                }
            }
        }
    });
}

#[tauri::command]
async fn start_recording(app: AppHandle, settings: Settings) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || begin(&app, settings, false))
        .await
        .map_err(|_| "Could not start the recorder.".to_string())?
}

fn stop(app: &AppHandle) {
    let id = {
        let recorder = app.state::<Recorder>();
        let mut session = recorder.lock();
        if !matches!(
            session.snapshot.status,
            Status::Starting | Status::Recording
        ) {
            return;
        }
        let Some(active) = session.active.as_ref() else {
            return;
        };
        active.stop.store(true, Ordering::Release);
        let id = active.id;
        session.snapshot.status = Status::Processing;
        emit(app, &session);
        id
    };
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let recorder = handle.state::<Recorder>();
        let session = recorder.lock();
        if session
            .active
            .as_ref()
            .is_some_and(|active| active.id == id)
            && session.snapshot.status == Status::Processing
        {
            hide_controller(&handle);
        }
    });
}

#[tauri::command]
fn stop_recording(app: AppHandle) {
    stop(&app);
}

#[tauri::command]
async fn copy_output(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let recorder = app.state::<Recorder>();
        let output = recorder
            .lock()
            .snapshot
            .last_output
            .clone()
            .ok_or("Record a video before copying a file.")?;
        copy_file(&app, &output.path)?;
        let mut session = recorder.lock();
        if let Some(last_output) = session
            .snapshot
            .last_output
            .as_mut()
            .filter(|last| last.path == output.path)
        {
            last_output.clipboard_copied = true;
            session.snapshot.error = None;
            emit(&app, &session);
        }
        Ok(())
    })
    .await
    .map_err(|_| "Could not access the clipboard.".to_string())?
}

#[tauri::command]
fn open_output_folder(app: AppHandle) -> Result<(), String> {
    let folder = video_folder(&app)?;
    let path: Vec<u16> = folder.as_os_str().encode_wide().chain([0]).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            path.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize <= 32 {
        return Err("Windows could not open the Videos folder.".into());
    }
    Ok(())
}

fn toggle_from_shortcut(app: AppHandle) {
    let recorder = app.state::<Recorder>();
    let (status, saved, incomplete) = {
        let session = recorder.lock();
        (
            session.snapshot.status,
            session.snapshot.settings.clone(),
            session.settings_incomplete,
        )
    };
    match status {
        Status::Starting | Status::Recording => stop(&app),
        Status::Processing => {}
        Status::Idle => {
            let result = match (incomplete, saved) {
                (false, Some(settings)) => begin(&app, settings, true),
                _ => Err("Finish choosing a capture source before recording.".into()),
            };
            if let Err(error) = result {
                let mut session = recorder.lock();
                if session.active.is_none() {
                    session.snapshot.error = Some(error);
                    emit(&app, &session);
                    drop(session);
                    if let Some(main) = app.get_webview_window("main") {
                        let _ = main.show();
                        let _ = main.unminimize();
                    }
                }
            }
        }
    }
}

pub fn run() {
    let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
    tauri::Builder::default()
        .plugin(
            tauri::plugin::Builder::<tauri::Wry, ()>::new("local-navigation")
                .on_navigation(|_, url| {
                    let port = url.port();
                    let bundled = matches!(
                        (url.scheme(), url.host_str(), port),
                        ("http", Some("tauri.localhost"), None)
                            | ("tauri", Some("localhost"), None)
                    );
                    let dev = cfg!(debug_assertions)
                        && (url.scheme(), url.host_str(), port)
                            == ("http", Some("localhost"), Some(1420));
                    (bundled || dev)
                        && url.username().is_empty()
                        && url.password().is_none()
                        && matches!(url.path(), "/" | "/index.html")
                })
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            let Some(recorder) = app.try_state::<Recorder>() else {
                return;
            };
            let (show_controls, show_main) = {
                let session = recorder.lock();
                let status = session.snapshot.status;
                (
                    status == Status::Recording
                        && session
                            .active
                            .as_ref()
                            .is_some_and(|active| active.show_controls),
                    status == Status::Idle
                        || (status == Status::Processing
                            && session
                                .active
                                .as_ref()
                                .is_some_and(|active| active.capture_ended)),
                )
            };
            if show_controls {
                show_controller(app);
            } else if show_main && let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.unminimize();
                let _ = main.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, event_shortcut, event| {
                    if *event_shortcut != shortcut || event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let handle = app.clone();
                    tauri::async_runtime::spawn_blocking(move || toggle_from_shortcut(handle));
                })
                .build(),
        )
        .setup(move |app| {
            let preferences_path = app.path().app_config_dir()?.join("preferences.json");
            let controller_excluded = app
                .get_webview_window("controller")
                .and_then(|controller| controller.hwnd().ok())
                .is_some_and(|hwnd| {
                    let mut affinity = 0;
                    unsafe {
                        SetWindowDisplayAffinity(hwnd.0.cast(), WDA_EXCLUDEFROMCAPTURE) != 0
                            && GetWindowDisplayAffinity(hwnd.0.cast(), &mut affinity) != 0
                            && affinity == WDA_EXCLUDEFROMCAPTURE
                    }
                });
            let videos = recording_folder(app.handle()).ok();
            app.manage(Recorder::load(
                preferences_path,
                videos.as_deref(),
                controller_excluded,
            ));
            let available = app.global_shortcut().register(shortcut).is_ok();
            app.state::<Recorder>().lock().snapshot.shortcut_available = available;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "controller" {
                    api.prevent_close();
                    stop(window.app_handle());
                } else if window.state::<Recorder>().lock().snapshot.status != Status::Idle {
                    api.prevent_close();
                    let _ = window.minimize();
                    stop(window.app_handle());
                } else if let Some(controller) =
                    window.app_handle().get_webview_window("controller")
                {
                    let _ = controller.destroy();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_targets,
            app_state,
            set_recording_settings,
            start_recording,
            stop_recording,
            copy_output,
            open_output_folder
        ])
        .run(tauri::generate_context!())
        .expect("could not start Zoop Go");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_replaced_or_truncated_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ffmpeg.exe");
        let expected = b"verified content";
        let hash: [u8; 32] = Sha256::digest(expected).into();
        fs::write(&path, expected).unwrap();
        assert!(verify_ffmpeg(&path, expected.len() as u64, &hash).unwrap());
        fs::write(&path, b"modified content").unwrap();
        assert!(!verify_ffmpeg(&path, expected.len() as u64, &hash).unwrap());
        fs::write(&path, b"truncated").unwrap();
        assert!(!verify_ffmpeg(&path, expected.len() as u64, &hash).unwrap());
    }

    #[test]
    fn validates_the_recent_recording_directory_and_starts_with_a_paused_shortcut() {
        let directory = tempfile::tempdir().unwrap();
        let videos = directory.path().join("Videos");
        fs::create_dir(&videos).unwrap();
        let filename = "Zoop Go example.mp4";
        let local = videos.join(filename);
        let elsewhere = directory.path().join(filename);
        for path in [&local, &elsewhere] {
            fs::write(path, b"clip").unwrap();
        }
        assert!(is_recording_file(&local, &videos));
        assert!(!is_recording_file(&elsewhere, &videos));

        let pref = directory.path().join("preferences.json");
        let preferences = Preferences {
            settings: None,
            last_output: Some(Output {
                path: elsewhere,
                filename: filename.to_string(),
                format: Format::Mp4,
                clipboard_copied: true,
            }),
        };
        fs::write(&pref, serde_json::to_vec(&preferences).unwrap()).unwrap();
        let recorder = Recorder::load(pref, Some(&videos), true);
        let session = recorder.lock();
        assert!(session.settings_incomplete);
        assert!(session.snapshot.last_output.is_none());
    }

    #[test]
    fn recovery_preserves_the_video_and_never_overwrites_a_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("scratch.mp4");
        let saved = directory.path().join("saved.mp4");
        fs::write(&raw, b"video").unwrap();
        preserve_original(&raw, &saved).unwrap();
        assert_eq!(fs::read(&saved).unwrap(), b"video");
        let other = directory.path().join("second.mp4");
        fs::write(&other, b"second").unwrap();
        assert!(preserve_original(&other, &saved).is_err());
        assert_eq!(fs::read(&saved).unwrap(), b"video");
    }
}
