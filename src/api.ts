import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";

export type CaptureKind = "monitor" | "window" | "region";
export type Format = "mp4" | "gif";
export type FrameRate = 30 | 60;
export type Rect = { x: number; y: number; width: number; height: number };
export type Target = Rect & { id: string; label: string; primary?: boolean };
export type Targets = { monitors: Target[]; windows: Target[] };
export type Settings = {
  target: { kind: CaptureKind; id: string; region?: Rect };
  fps: FrameRate;
  format: Format;
  autoCopy: boolean;
};

export type Output = {
  path: string;
  filename: string;
  format: Format;
  clipboardCopied: boolean;
};

export type AppState = {
  status: "idle" | "starting" | "recording" | "processing";
  startedAt?: number | null;
  settings?: Settings | null;
  shortcutAvailable?: boolean;
  lastOutput?: Output | null;
  error?: string | null;
};

export const api = {
  targets: () => invoke<Targets>("list_targets"),
  state: () => invoke<AppState>("app_state"),
  saveSettings: (settings: Settings | null) => invoke("set_recording_settings", { settings }),
  start: (settings: Settings) => invoke("start_recording", { settings }),
  stop: () => invoke("stop_recording"),
  copy: () => invoke("copy_output"),
  openFolder: () => invoke("open_output_folder"),
};

export function useRecorder(windowLabel: "main" | "controller" = "main") {
  const [state, setState] = useState<AppState>({ status: "idle" });
  const [ready, setReady] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    const eventName = windowLabel === "controller" ? "recording-controls-state" : "recording-state";
    let active = true;
    let revision = 0;
    let unlisten: (() => void) | undefined;
    let retry: number | undefined;

    async function connect() {
      let requestedAt = revision;
      try {
        if (!unlisten) {
          const unsubscribe = await listen<AppState>(eventName, ({ payload }) => {
            if (!active) return;
            revision += 1;
            setError("");
            setState(payload);
            setReady(true);
          }, { target: windowLabel });
          if (!active) {
            await unsubscribe();
            return;
          }
          unlisten = unsubscribe;
        }
        requestedAt = revision;
        const initialState = await api.state();
        if (!active || revision !== requestedAt) return;
        setError("");
        setState(initialState);
        setReady(true);
      } catch (failure) {
        if (!active || revision !== requestedAt) return;
        setError(errorText(failure, "Couldn't connect to the recorder."));
        retry = window.setTimeout(() => void connect(), 1500);
      }
    }

    void connect();

    return () => {
      active = false;
      window.clearTimeout(retry);
      if (unlisten) void Promise.resolve().then(unlisten).catch(() => {});
    };
  }, [windowLabel]);

  return { state, ready, error };
}

export function useElapsed(startedAt?: number | null) {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (startedAt == null) return;
    setNow(Date.now());
    const id = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(id);
  }, [startedAt]);

  const seconds = startedAt == null ? 0 : Math.max(0, Math.floor((now - startedAt) / 1000));
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const pad = (value: number) => String(value).padStart(2, "0");
  return (hours ? hours + ":" : "") + pad(minutes % 60) + ":" + pad(seconds % 60);
}

export function errorText(error: unknown, fallback: string) {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error && error.message.trim()) return error.message;
  return fallback;
}
