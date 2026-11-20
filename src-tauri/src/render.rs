use crate::process::{read_diagnostic, wait_for_child};
use std::{
    fs,
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use tempfile::TempPath;

const EXPORT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(120);
const PALETTE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug)]
pub struct Click {
    pub time: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mp4,
    Gif,
}

pub fn export(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    width: u32,
    height: u32,
    fps: u32,
    clicks: &[Click],
    format: Format,
    auto_zoom: bool,
) -> Result<(), String> {
    if width < 8 || height < 8 || fps == 0 {
        return Err("The recording has invalid dimensions or frame rate.".into());
    }
    let directory = output
        .parent()
        .ok_or("The export needs a destination folder.")?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("Could not prepare the export folder. {error}"))?;
    let work = tempfile::Builder::new()
        .prefix(".zoop-go-")
        .tempdir_in(directory)
        .map_err(|error| format!("Could not prepare the export folder. {error}"))?;
    let work_path = work.path();
    let matte = Matte::new(width, height, format);
    fs::write(work_path.join("matte.rgba"), matte.image())
        .map_err(|error| format!("Could not prepare the video background. {error}"))?;

    let result_fps = match format {
        Format::Mp4 => fps,
        Format::Gif => fps.min(15),
    };
    let timeline = if auto_zoom {
        camera_timeline(clicks, result_fps)
    } else {
        Vec::new()
    };
    let graph = matte.graph(result_fps, format == Format::Gif, &timeline);

    let mut command = ffmpeg_command(ffmpeg, input, work_path, &matte);
    let graph = match format {
        Format::Mp4 => format!("{graph};[composite]format=yuv420p[result]"),
        Format::Gif => {
            // Use a global palette without buffering every frame of the recording.
            let sample_graph = matte.graph(5, true, &[]);
            let palette_graph =
                format!("{sample_graph};[composite]palettegen=stats_mode=diff[result]");
            let mut palette_command = ffmpeg_command(ffmpeg, input, work_path, &matte);
            add_graph(&mut palette_command, work_path, &palette_graph)?;
            let palette = work_path.join("palette.bgra");
            palette_command
                .args([
                    "-frames:v",
                    "1",
                    "-pix_fmt",
                    "bgra",
                    "-c:v",
                    "rawvideo",
                    "-f",
                    "rawvideo",
                ])
                .arg(&palette);
            run(
                palette_command,
                "Could not create the GIF color palette.",
                PALETTE_PROGRESS_TIMEOUT,
            )?;

            add_raster_input(&mut command, &palette, "bgra", "16x16");
            format!(
                "{graph};[composite][2:v]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle[result]"
            )
        }
    };
    add_graph(&mut command, work_path, &graph)?;
    match format {
        Format::Mp4 => {
            command.args([
                "-c:v",
                "libx264",
                "-threads",
                "6",
                "-preset",
                "veryfast",
                "-crf",
                "20",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ]);
        }
        Format::Gif => {
            command.args(["-loop", "0", "-f", "gif"]);
        }
    }
    let staged = work_path.join("finished");
    command.arg(&staged);
    run(
        command,
        "Could not encode the recording.",
        EXPORT_PROGRESS_TIMEOUT,
    )?;
    let length = fs::metadata(&staged).map(|metadata| metadata.len());
    if !matches!(length, Ok(length) if length > 0) {
        return Err("The export did not contain a video.".into());
    }
    TempPath::try_from_path(staged)
        .and_then(|staged| {
            staged
                .persist_noclobber(output)
                .map_err(|error| error.error)
        })
        .map_err(|error| format!("Could not save the export. {error}"))
}

fn ffmpeg_command(ffmpeg: &Path, input: &Path, work: &Path, matte: &Matte) -> Command {
    let mut command = Command::new(ffmpeg);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW

    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-nostats",
            "-progress",
            "pipe:1",
            "-filter_complex_threads",
            "4",
            "-y",
            "-threads",
            "4",
            "-i",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    add_raster_input(
        &mut command,
        &work.join("matte.rgba"),
        "rgba",
        &format!("{}x{}", matte.width, matte.height),
    );
    command
}

fn add_raster_input(command: &mut Command, path: &Path, format: &str, size: &str) {
    command
        .args([
            "-f",
            "rawvideo",
            "-pixel_format",
            format,
            "-video_size",
            size,
            "-framerate",
            "30",
            "-i",
        ])
        .arg(path);
}

fn add_graph(command: &mut Command, work: &Path, graph: &str) -> Result<(), String> {
    let path = work.join("graph.txt");
    fs::write(&path, graph)
        .map_err(|error| format!("Could not prepare the video filters. {error}"))?;
    command
        .arg("-/filter_complex")
        .arg(path)
        .args(["-map", "[result]", "-an"]);
    Ok(())
}

fn run(mut command: Command, message: &str, idle_limit: Duration) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the video encoder. {error}"))?;
    let mut progress = child
        .stdout
        .take()
        .expect("encoder stdout was configured as piped");
    let diagnostic = child
        .stderr
        .take()
        .expect("encoder stderr was configured as piped");
    let start = Instant::now();
    let last_progress = AtomicU64::new(0);
    let (status, stderr) = thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut chunk = [0; 1024];
            loop {
                match progress.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(_) => {
                        last_progress.store(start.elapsed().as_millis() as u64, Ordering::Relaxed)
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        let logs = scope.spawn(|| read_diagnostic(diagnostic));
        let status = wait_for_child(&mut child, || {
            let last = Duration::from_millis(last_progress.load(Ordering::Relaxed));
            (start.elapsed().saturating_sub(last) >= idle_limit)
                .then_some("The video encoder stopped reporting progress.")
        });
        let _ = reader.join();
        (status, logs.join().unwrap_or_default())
    });
    let status = status.map_err(|error| format!("{message} {error}"))?;
    if status.success() {
        return Ok(());
    }
    let detail = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .take(240)
        .collect::<String>();
    Err(format!("{message} {}", detail.trim()).trim_end().into())
}

struct Matte {
    width: u32,
    height: u32,
    left: u32,
    top: u32,
    content_width: u32,
    content_height: u32,
}

impl Matte {
    fn new(source_width: u32, source_height: u32, format: Format) -> Self {
        let (long_limit, short_limit) = match format {
            Format::Mp4 => (1920.0, 1080.0),
            Format::Gif => (960.0, 540.0),
        };
        let source_width = f64::from(source_width);
        let source_height = f64::from(source_height);
        let fit = (long_limit / source_width.max(source_height))
            .min(short_limit / source_width.min(source_height))
            .min(1.0);
        let width = even(source_width * fit);
        let height = even(source_height * fit);
        let inset = f64::from(width.min(height)) * 0.072;
        let inside = (f64::from(width) - 2.0 * inset) / source_width;
        let inside = inside.min((f64::from(height) - 2.0 * inset) / source_height);
        let left = even((f64::from(width) - source_width * inside) * 0.5).min((width - 2) / 2);
        let top = even((f64::from(height) - source_height * inside) * 0.5).min((height - 2) / 2);
        let left = left & !1;
        let top = top & !1;
        Self {
            width,
            height,
            left,
            top,
            content_width: width - 2 * left,
            content_height: height - 2 * top,
        }
    }

    fn image(&self) -> Vec<u8> {
        let mut image = vec![0; self.width as usize * self.height as usize * 4];
        let half_width = self.content_width as f32 * 0.5;
        let half_height = self.content_height as f32 * 0.5;
        let center_x = self.left as f32 + half_width;
        let center_y = self.top as f32 + half_height;
        let radius = (half_width.min(half_height) * 0.044).clamp(1.0, 22.0);
        let width = self.width as usize;
        let height = self.height as usize;

        for (y, row) in image.chunks_exact_mut(width * 4).enumerate() {
            let dy = (y as f32 + 0.5 - center_y).abs() - half_height + radius;
            let v = y as f32 / height as f32;
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let dx = (x as f32 + 0.5 - center_x).abs() - half_width + radius;
                if dx <= 0.0 && dy <= 0.0 {
                    continue;
                }
                let distance = dx.max(dy).min(0.0) + dx.max(0.0).hypot(dy.max(0.0)) - radius;
                let alpha = (distance + 0.5).clamp(0.0, 1.0);
                if alpha == 0.0 {
                    continue;
                }
                let u = x as f32 / width as f32;
                let light = (1.0 - (u - 0.79).abs() * 1.3 - v * 0.75).max(0.0);
                let glow = light * light;
                let shadow = 1.0 - 0.34 * (-distance.max(0.0) / (radius * 0.75)).exp();
                let edge = 16.0 * (1.0 - distance.abs()).max(0.0);
                pixel[0] = ((15.0 + 30.0 * glow + 7.0 * u) * shadow + edge) as u8;
                pixel[1] = ((20.0 + 41.0 * glow + 6.0 * u) * shadow + edge) as u8;
                pixel[2] = ((44.0 + 82.0 * glow + 9.0 * u) * shadow + edge) as u8;
                pixel[3] = (alpha * 255.0).round() as u8;
            }
        }
        image
    }

    fn graph(&self, fps: u32, resample: bool, timeline: &[Keyframe]) -> String {
        let prefix = if resample {
            format!("fps={fps}:start_time=0:round=up,")
        } else {
            String::new()
        };
        let w = self.content_width;
        let h = self.content_height;
        let camera = if timeline.is_empty() {
            format!("scale={w}:{h}:flags=fast_bilinear")
        } else {
            let zoom = camera_expression(timeline, |camera| camera.zoom);
            let x = camera_expression(timeline, |camera| camera.x);
            let y = camera_expression(timeline, |camera| camera.y);
            format!(
                "zoompan=z='{zoom}':x='iw*(({x})-0.5/zoom)':y='ih*(({y})-0.5/zoom)':d=1:s={w}x{h}:fps={fps}"
            )
        };

        format!(
            "[0:v]{prefix}{camera},setsar=1,pad={}:{}:{}:{}:color=black[video];\n[video][1:v]overlay=eof_action=repeat:repeatlast=1[composite]",
            self.width, self.height, self.left, self.top
        )
    }
}

fn even(dimension: f64) -> u32 {
    (dimension.round() as u32 & !1).max(2)
}

#[derive(Clone, Copy)]
struct Camera {
    zoom: f64,
    x: f64,
    y: f64,
}

const FIT: Camera = Camera {
    zoom: 1.0,
    x: 0.5,
    y: 0.5,
};

struct Keyframe {
    frame: u64,
    camera: Camera,
}

fn camera_timeline(clicks: &[Click], fps: u32) -> Vec<Keyframe> {
    let frames = |seconds| (seconds * f64::from(fps) + 0.5) as u64;
    let approach = frames(0.34).max(1);
    let hold = frames(0.84);
    let retreat = frames(0.43).max(1);
    let minimum_click_spacing = frames(0.13).max(1);
    let mut points = clicks
        .iter()
        .filter(|click| {
            click.time.is_finite()
                && click.time >= 0.0
                && click.x.is_finite()
                && click.y.is_finite()
        })
        .map(|click| Keyframe {
            frame: frames(click.time),
            camera: camera_target(click),
        })
        .collect::<Vec<_>>();
    points.sort_by_key(|point| point.frame);
    points.dedup_by(|later, earlier| later.frame - earlier.frame < minimum_click_spacing);
    if points.is_empty() {
        return Vec::new();
    }

    let mut result = vec![Keyframe {
        frame: 0,
        camera: FIT,
    }];
    let mut active = FIT;
    for (index, point) in points.iter().enumerate() {
        let next = points
            .get(index + 1)
            .map(|point| point.frame)
            .unwrap_or(u64::MAX);
        let arrive = point.frame.saturating_add(approach).min(next);
        let leave = arrive.saturating_add(hold);

        if point.frame > result.last().unwrap().frame {
            result.push(Keyframe {
                frame: point.frame,
                camera: active,
            });
        }
        result.push(Keyframe {
            frame: arrive,
            camera: point.camera,
        });
        active = point.camera;

        let reset = leave.saturating_add(retreat);
        if reset <= next && reset > arrive {
            result.push(Keyframe {
                frame: leave,
                camera: active,
            });
            result.push(Keyframe {
                frame: reset,
                camera: FIT,
            });
            active = FIT;
        }
    }
    result
}

fn camera_target(click: &Click) -> Camera {
    let zoom = 1.52;
    let half_crop = 0.5 / zoom;
    Camera {
        zoom,
        x: click.x.clamp(half_crop, 1.0 - half_crop),
        y: click.y.clamp(half_crop, 1.0 - half_crop),
    }
}

fn camera_expression(keyframes: &[Keyframe], value: impl Fn(Camera) -> f64 + Copy) -> String {
    let Some((start, rest)) = keyframes.split_first() else {
        return String::new();
    };
    match rest {
        [] => format!("{:.6}", value(start.camera)),
        [end] => {
            let from = value(start.camera);
            let to = value(end.camera);
            if from == to {
                return format!("{from:.6}");
            }
            let delta = to - from;
            let length = end.frame - start.frame;
            format!(
                "st(0,clip((on-{})/{length},0,1));{from:.6}+({delta:.6})*ld(0)^2*(3-2*ld(0))",
                start.frame
            )
        }
        _ => {
            // A balanced expression keeps FFmpeg's evaluation depth bounded
            // even when a recording has thousands of clicks.
            let middle = keyframes.len() / 2;
            let division = keyframes[middle].frame;
            let lower = camera_expression(&keyframes[..=middle], value);
            let upper = camera_expression(&keyframes[middle..], value);
            format!("if(lt(on,{division}),{lower},{upper})")
        }
    }
}
