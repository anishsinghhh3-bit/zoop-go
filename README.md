# Zoop Go

Zoop Go is a local Windows screen recorder for short demos and tutorials. It adds smooth zooms around mouse clicks and places the recording on a padded, rounded background. Version 1.0.1 runs offline without an account or upload.

## Support Zoop Go

If Zoop Go helps you, please consider supporting its development through [my website](https://flowv-studio.unhurriedlorikeet.chatgpt.site/).

## Install

Zoop Go supports x64 PCs running Windows 10 22H2 or Windows 11.

- **Offline installer.** Run the setup executable. It includes the Microsoft Edge WebView2 Runtime installer for PCs that need it.
- **Portable ZIP.** Extract the entire archive and run `Zoop Go.exe` from the extracted folder. This edition uses the system Microsoft Edge WebView2 Runtime.

## Record and share

1. Choose **Display**, **Window**, or **Region**. For a region, draw on the display map or enter the position and size in pixels.
2. Choose **30 fps** or **60 fps**, then select **MP4** or **GIF**.
3. Select **Start recording** or press **Ctrl+Shift+R**. Use the same shortcut or select **Stop** when you finish.
4. After export, use **Open folder** to find the file or **Copy file** to copy it again.

Zoop Go saves finished files in the `Zoop Go` folder inside your Windows Videos folder. The default path is `%USERPROFILE%\Videos\Zoop Go`. With **Copy file after export** enabled, Zoop Go puts the finished file on the Windows clipboard. Paste it where your destination supports file pasting. You can also attach it directly from the folder. If polishing fails after capture, Zoop Go saves the original recording where possible and shows where to find it.

Recordings are silent. MP4 exports fit within 1920 × 1080 at the selected frame rate. GIF exports fit within 960 × 540 at 15 fps. Higher resolutions and longer recordings can take more time to export.

## Build from source

Build on Windows x64 with the Rust MSVC toolchain (`x86_64-pc-windows-msvc`), Visual Studio Build Tools with **Desktop development with C++**, Node.js 22 or later with npm, Git for Windows, and the Microsoft Edge WebView2 Runtime. [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#windows)

Open PowerShell in the project root and run these commands.

```powershell
npm ci
.\scripts\Build-FFmpeg.ps1
npm run tauri -- dev
```

The FFmpeg script checks its pinned downloads and builds the sidecar from a fresh staging folder before building Zoop Go. You can skip that step if the source tree already contains the matching x64 sidecar. Release packages include FFmpeg.

Run the following command to create a Windows release build.

```powershell
npm run tauri -- build
```

The build produces an NSIS setup executable in the `bundle\nsis` directory under the Cargo release output. The bundle uses the offline WebView2 installation mode. [Tauri Windows installer documentation](https://v2.tauri.app/distribute/windows-installer/#webview2-installation-options)

## License

Zoop Go source is available under the [MIT License](LICENSE). Bundled components, including FFmpeg, have their own licenses and notices.
