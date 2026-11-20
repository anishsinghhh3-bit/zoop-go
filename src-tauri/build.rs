use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=windows.manifest");
    let sidecar = PathBuf::from("binaries").join(format!(
        "ffmpeg-{}.exe",
        env::var("TARGET").expect("Cargo target")
    ));
    println!("cargo:rerun-if-changed={}", sidecar.display());
    let binary = fs::read(&sidecar)
        .unwrap_or_else(|error| panic!("read bundled FFmpeg at {}: {error}", sidecar.display()));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(output.join("ffmpeg.sha256"), Sha256::digest(&binary))
        .expect("write bundled FFmpeg hash");
    println!("cargo:rustc-env=ZOOP_FFMPEG_BYTES={}", binary.len());

    let windows =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows.manifest"));
    let commands = tauri_build::AppManifest::new().commands(&[
        "list_targets",
        "app_state",
        "set_recording_settings",
        "start_recording",
        "stop_recording",
        "copy_output",
        "open_output_folder",
    ]);
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(windows)
            .app_manifest(commands),
    )
    .expect("build Tauri resources");
}
