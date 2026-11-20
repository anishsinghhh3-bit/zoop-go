import {
  Aperture, AppWindow, Check, ChevronDown, CircleAlert, Clipboard, FolderOpen,
  Monitor, MousePointer2, RefreshCw, ScanLine, ShieldCheck, Square, X,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, ReactNode } from "react";
import { api, errorText, useElapsed, useRecorder } from "./api";
import type { AppState, CaptureKind, Format, FrameRate, Rect, Settings, Target, Targets } from "./api";
import { defaultRegion, MIN_REGION, pixels, validRegion } from "./region";
import { createShortcutSettingsQueue } from "./shortcut-settings";
import type { ShortcutSettingsResult } from "./shortcut-settings";

const captureModes = [
  { value: "monitor", label: "Display", icon: Monitor },
  { value: "window", label: "Window", icon: AppWindow },
  { value: "region", label: "Region", icon: ScanLine },
] as const;
const frameRates = [{ value: 30, label: "30 fps" }, { value: 60, label: "60 fps" }] as const;
const formats = [{ value: "mp4", label: "MP4" }, { value: "gif", label: "GIF" }] as const;

function Segmented<T extends string | number>({
  label, options, value, onChange, disabled,
}: {
  label: string;
  options: ReadonlyArray<{ value: T; label: string; icon?: typeof Monitor }>;
  value: T;
  onChange: (value: T) => void;
  disabled?: boolean;
}) {
  return (
    <div className="segments" role="group" aria-label={label}>
      {options.map(({ value: option, label: name, icon: Icon }) => (
        <button
          type="button"
          key={option}
          aria-pressed={value === option}
          onClick={() => onChange(option)}
          disabled={disabled}
        >
          {Icon && <Icon size={15} aria-hidden="true" />}
          {name}
        </button>
      ))}
    </div>
  );
}

function Shortcut() {
  return (
    <span className="shortcut" aria-label="Control plus Shift plus R">
      <kbd>Ctrl</kbd><kbd>Shift</kbd><kbd>R</kbd>
    </span>
  );
}

function Status({ status, elapsed, idleLabel }: { status: AppState["status"]; elapsed: string; idleLabel: string }) {
  const label = {
    idle: idleLabel,
    starting: "Starting",
    recording: "Recording",
    processing: "Exporting",
  }[status];

  return (
    <span className={"status status--" + status}>
      <span className="status-dot" aria-hidden="true" />
      <span role="status">{label}</span>
      {status === "recording" && <time aria-label="Elapsed time">{elapsed}</time>}
    </span>
  );
}

type PreviewProps = {
  kind: CaptureKind;
  target?: Target;
  region: Rect | null;
  onRegion: (region: Rect) => void;
  drawing: boolean;
  onDrawingChange: (drawing: boolean) => void;
  status: AppState["status"];
  disabled: boolean;
};

function CapturePreview({ kind, target, region, onRegion, drawing, onDrawingChange, status, disabled }: PreviewProps) {
  const anchor = useRef<{ from: { x: number; y: number }; bounds: DOMRect } | null>(null);
  const showRegion = kind === "region" && target && region && validRegion(region, target);
  const editable = kind === "region" && target && !disabled;
  const aspect = target ? Math.max(0.28, Math.min(4.5, target.width / target.height)) : 16 / 9;
  const viewWidth = Math.min(340, Math.round(174 * aspect));
  const size = showRegion ? region : target;

  function point(event: ReactPointerEvent<HTMLDivElement>, bounds: DOMRect) {
    return {
      x: Math.round(Math.max(0, Math.min(1, (event.clientX - bounds.left) / bounds.width)) * (target?.width ?? 0)),
      y: Math.round(Math.max(0, Math.min(1, (event.clientY - bounds.top) / bounds.height)) * (target?.height ?? 0)),
    };
  }

  function updateSelection(event: ReactPointerEvent<HTMLDivElement>) {
    if (!target || !anchor.current) return;
    const { from, bounds } = anchor.current;
    const to = point(event, bounds);
    const width = Math.abs(from.x - to.x);
    const height = Math.abs(from.y - to.y);
    if (width < MIN_REGION || height < MIN_REGION) return;
    onRegion({ x: Math.min(from.x, to.x), y: Math.min(from.y, to.y), width, height });
  }

  return (
    <section className="panel frame-panel" aria-labelledby="area-title">
      <header className="panel-title">
        <h2 id="area-title">Capture area</h2>
        {size && <span className="dimensions">{pixels(size)} px</span>}
      </header>
      <div className={"stage stage--" + status}>
        <span className="stage-corner stage-corner--top" aria-hidden="true" />
        <span className="stage-corner stage-corner--bottom" aria-hidden="true" />
        {target ? (
          <div
            className={"screen" + (kind === "window" ? " screen--window" : "") + (editable ? " screen--editable" : "")}
            style={{ width: viewWidth, aspectRatio: aspect }}
            aria-hidden="true"
            onPointerDown={(event) => {
              if (!editable || event.button !== 0 || !event.isPrimary) return;
              event.currentTarget.setPointerCapture(event.pointerId);
              const bounds = event.currentTarget.getBoundingClientRect();
              anchor.current = { from: point(event, bounds), bounds };
              onDrawingChange(true);
            }}
            onPointerMove={(event) => {
              if (editable && event.isPrimary) updateSelection(event);
            }}
            onPointerUp={(event) => {
              if (editable && event.isPrimary) updateSelection(event);
              anchor.current = null;
              onDrawingChange(false);
            }}
            onLostPointerCapture={() => {
              anchor.current = null;
              onDrawingChange(false);
            }}
          >
            {kind !== "region" && (
              <div className="screen-icon">
                {kind === "window" ? <AppWindow size={32} strokeWidth={1.45} /> : <Monitor size={33} strokeWidth={1.45} />}
              </div>
            )}
            {showRegion && (
              <div
                className={"selection" + (drawing ? " selection--drawing" : "")}
                style={{
                  left: (100 * region.x / target.width) + "%",
                  top: (100 * region.y / target.height) + "%",
                  width: (100 * region.width / target.width) + "%",
                  height: (100 * region.height / target.height) + "%",
                }}
              >
                <span /><span /><span /><span />
                <ScanLine size={26} strokeWidth={1.45} />
              </div>
            )}
          </div>
        ) : (
          <div className="stage-empty">
            <ScanLine size={29} strokeWidth={1.45} aria-hidden="true" />
            <span>No source selected</span>
          </div>
        )}
      </div>
      <p className="panel-note">
        {kind === "region" ? (
          <><ScanLine size={15} aria-hidden="true" /> Draw on the display map or enter its position and size.</>
        ) : (
          <><MousePointer2 size={15} aria-hidden="true" /> Clicks get a smooth zoom in the exported recording.</>
        )}
      </p>
    </section>
  );
}

function FrameSetting({ children, label }: { children: ReactNode; label: string }) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      {children}
    </div>
  );
}

export function App() {
  const recorder = useRecorder();
  const { state } = recorder;
  const [targets, setTargets] = useState<Targets>({ monitors: [], windows: [] });
  const [targetsBusy, setTargetsBusy] = useState(true);
  const [targetError, setTargetError] = useState("");
  const [kind, setKind] = useState<CaptureKind>("monitor");
  const [monitorId, setMonitorId] = useState("");
  const [windowId, setWindowId] = useState("");
  const [region, setRegion] = useState<Rect | null>(null);
  const [drawingRegion, setDrawingRegion] = useState(false);
  const [fps, setFps] = useState<FrameRate>(30);
  const [format, setFormat] = useState<Format>("mp4");
  const [autoCopy, setAutoCopy] = useState(true);
  const [hasSettings, setHasSettings] = useState(false);
  const [pending, setPending] = useState<"start" | "stop" | "copy" | "folder" | null>(null);
  const [actionError, setActionError] = useState("");
  const [dismissedError, setDismissedError] = useState("");
  const [copiedPath, setCopiedPath] = useState("");
  const [shortcutSettings, setShortcutSettings] = useState<ShortcutSettingsResult | null>(null);
  const syncShortcutSettings = useMemo(() => createShortcutSettingsQueue(api.saveSettings, setShortcutSettings), []);
  const loadingTargets = useRef(0);
  const elapsed = useElapsed(state.startedAt);

  useEffect(() => {
    if (!recorder.ready || hasSettings) return;
    const saved = state.settings;
    if (saved) {
      setKind(saved.target.kind);
      if (saved.target.kind === "window") setWindowId(saved.target.id);
      else setMonitorId(saved.target.id);
      setRegion(saved.target.region ?? null);
      setFps(saved.fps);
      setFormat(saved.format);
      setAutoCopy(saved.autoCopy);
    }
    setHasSettings(true);
  }, [recorder.ready, state.settings, hasSettings]);

  const refresh = useCallback(async () => {
    const request = ++loadingTargets.current;
    setTargetsBusy(true);
    setTargetError("");
    try {
      const available = await api.targets();
      if (request === loadingTargets.current) setTargets(available);
    } catch (error) {
      if (request === loadingTargets.current) {
        setTargets({ monitors: [], windows: [] });
        setTargetError(errorText(error, "Couldn't find capture sources."));
      }
    } finally {
      if (request === loadingTargets.current) setTargetsBusy(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    return () => { loadingTargets.current += 1; };
  }, [refresh]);

  useEffect(() => {
    if (!hasSettings || targetsBusy || monitorId) return;
    const firstMonitor = targets.monitors.find(({ primary }) => primary) ?? targets.monitors[0];
    if (firstMonitor) setMonitorId(firstMonitor.id);
  }, [hasSettings, monitorId, targets.monitors, targetsBusy]);

  const currentMonitor = targets.monitors.find(({ id }) => id === monitorId);
  const currentWindow = targets.windows.find(({ id }) => id === windowId);
  const target = kind === "window" ? currentWindow : currentMonitor;
  const currentRegion = region ?? (currentMonitor ? defaultRegion(currentMonitor) : null);
  const regionIsValid = kind !== "region" || Boolean(target && currentRegion && validRegion(currentRegion, target));
  const settings: Settings | null = target && regionIsValid && (kind !== "region" || currentRegion) ? {
    target: { kind, id: target.id, ...(kind === "region" && currentRegion ? { region: currentRegion } : {}) },
    fps,
    format,
    autoCopy,
  } : null;
  const actionLocked = pending === "start" || pending === "stop";
  const busy = state.status !== "idle" || actionLocked;
  const status = state.status === "idle" && pending === "start" ? "starting" : state.status;
  const canStart = recorder.ready && hasSettings && Boolean(settings) && !drawingRegion && !busy && !targetsBusy && pending === null;
  const idleLabel = !recorder.ready ? "Connecting" : targetsBusy ? "Finding sources" : !target ? "Choose a source" : drawingRegion || !regionIsValid ? "Set a region" : "Ready";
  const shortcutKey = recorder.ready && hasSettings && !busy ? JSON.stringify(targetsBusy || drawingRegion ? null : settings) : undefined;
  const applyShortcutSettings = useCallback((key: string) => {
    setShortcutSettings(null);
    void syncShortcutSettings(key, JSON.parse(key) as Settings | null);
  }, [syncShortcutSettings]);

  useLayoutEffect(() => {
    if (shortcutKey !== undefined) applyShortcutSettings(shortcutKey);
  }, [applyShortcutSettings, shortcutKey]);

  const recent = state.lastOutput;
  const clipboardCopied = Boolean(recent && (recent.clipboardCopied || recent.path === copiedPath));
  const runtimeError = state.error ?? recorder.error ?? "";
  const shortcutError = shortcutSettings && shortcutSettings.key === shortcutKey && shortcutSettings.status === "failed" ?
    errorText(shortcutSettings.error, "Couldn't update the recording shortcut.") : "";
  const shortcutReady = shortcutKey !== "null" && shortcutSettings !== null &&
    shortcutSettings.key === shortcutKey && shortcutSettings.status === "applied";
  const error = actionError || (runtimeError !== dismissedError ? runtimeError : "") || targetError || shortcutError;

  async function perform(action: NonNullable<typeof pending>, work: () => Promise<unknown>, fallback: string) {
    setActionError("");
    setDismissedError("");
    setPending(action);
    try {
      await work();
    } catch (failure) {
      setActionError(errorText(failure, fallback));
    } finally {
      setPending(null);
    }
  }

  const options = kind === "window" ? targets.windows : targets.monitors;
  return (
    <>
      <header className="app-header">
        <div className="wordmark">
          <span className="brand-mark"><Aperture size={23} strokeWidth={2.15} aria-hidden="true" /></span>
          Zoop Go
        </div>
        <span className="local-indicator">
          <ShieldCheck size={15} strokeWidth={1.9} aria-hidden="true" />
          Local recorder
        </span>
      </header>

      <main className="app">
        <div className="app-intro">
          <div>
            <h1>Record your screen</h1>
            <p>Capture a display, window, or region and share the finished file.</p>
          </div>
          <Status status={status} elapsed={elapsed} idleLabel={idleLabel} />
        </div>

        {error && (
          <div className="notice" role="alert">
            <CircleAlert size={17} aria-hidden="true" />
            <span>{error}</span>
            {error === shortcutError && shortcutKey !== undefined ? (
              <button type="button" className="plain-button" onClick={() => applyShortcutSettings(shortcutKey)}>Retry</button>
            ) : (
              <button
                type="button"
                className="icon-button"
                onClick={() => { setActionError(""); setTargetError(""); setDismissedError(runtimeError); }}
                aria-label="Dismiss message"
              >
                <X size={16} aria-hidden="true" />
              </button>
            )}
          </div>
        )}

        <div className="workspace">
          <CapturePreview
            kind={kind} target={target} region={currentRegion} onRegion={setRegion} drawing={drawingRegion}
            onDrawingChange={setDrawingRegion} status={status} disabled={busy || targetsBusy}
          />
          <section className="panel options-panel" aria-labelledby="settings-title">
            <header className="panel-title"><h2 id="settings-title">Recording setup</h2></header>
            <div className="options-body">
              <FrameSetting label="Capture">
                <Segmented label="Capture type" options={captureModes} value={kind} disabled={busy} onChange={(value) => {
                  setKind(value);
                  if (value === "window") void refresh();
                }} />
              </FrameSetting>
              <div className="source-row">
                <label className="select-wrap">
                  <span className="sr-only">{kind === "window" ? "Window" : "Display"}</span>
                  <select
                    value={target?.id ?? ""}
                    disabled={busy || targetsBusy || options.length === 0}
                    onChange={(event) => {
                      if (kind === "window") setWindowId(event.target.value);
                      else {
                        setMonitorId(event.target.value);
                        setRegion(null);
                      }
                    }}
                  >
                    {!target && <option value="">{targetsBusy ? "Finding sources…" : kind === "window" ? "Choose a window" : options.length ? "Choose a display" : "No displays available"}</option>}
                    {options.map(({ id, label }) => <option key={id} value={id}>{label}</option>)}
                  </select>
                  <ChevronDown size={15} aria-hidden="true" />
                </label>
                <button
                  className="icon-button refresh-button"
                  type="button"
                  onClick={() => { void refresh(); }}
                  aria-label="Refresh capture sources"
                  title="Refresh capture sources"
                  disabled={targetsBusy || busy}
                >
                  <RefreshCw size={16} className={targetsBusy ? "spin" : undefined} aria-hidden="true" />
                </button>
              </div>

              {kind === "region" && target && currentRegion && (
                <>
                  <div className="region-fields" role="group" aria-label="Region position and size">
                    {([["x", "X"], ["y", "Y"], ["width", "Width"], ["height", "Height"]] as const).map(([key, label]) => (
                      <label key={key}>
                        <span>{label}</span>
                        <input
                          type="number"
                          inputMode="numeric"
                          value={Number.isNaN(currentRegion[key]) ? "" : currentRegion[key]}
                          min={key === "x" || key === "y" ? 0 : MIN_REGION}
                          max={key === "x" || key === "width" ? target.width : target.height}
                          onChange={(event) => setRegion({ ...currentRegion, [key]: event.target.valueAsNumber })}
                          disabled={busy}
                          aria-invalid={!regionIsValid}
                        />
                      </label>
                    ))}
                  </div>
                  {!regionIsValid && <p className="field-error">Use an area inside the display, at least 64 × 64 px.</p>}
                </>
              )}

              <div className="format-row">
                <FrameSetting label="Frame rate">
                  <Segmented label="Frame rate" options={frameRates} value={fps} disabled={busy} onChange={setFps} />
                </FrameSetting>
                <FrameSetting label="Export">
                  <Segmented label="Export format" options={formats} value={format} disabled={busy} onChange={setFormat} />
                </FrameSetting>
              </div>

              <label className="auto-copy">
                <span>Copy file after export</span>
                <input type="checkbox" checked={autoCopy} disabled={busy} onChange={(event) => setAutoCopy(event.target.checked)} />
                <span className="switch" aria-hidden="true" />
              </label>

              <div className="record-controls">
                {state.status === "recording" ? (
                  <button
                    className="record-button record-button--stop"
                    type="button"
                    disabled={pending === "stop"}
                    onClick={() => void perform("stop", api.stop, "Couldn't stop the recording.")}
                  >
                    <Square size={15} fill="currentColor" aria-hidden="true" />
                    {pending === "stop" ? "Stopping…" : "Stop recording"}
                  </button>
                ) : (
                  <button
                    type="button"
                    className="record-button"
                    disabled={!canStart}
                    onClick={() => {
                      if (settings) void perform("start", () => api.start(settings), "Couldn't start recording.");
                    }}
                  >
                    {status === "starting" || status === "processing" ?
                      <RefreshCw size={16} className="spin" aria-hidden="true" /> :
                      <span className="record-icon" aria-hidden="true" />}
                    {status === "starting" ? "Starting…" : status === "processing" ? "Exporting…" : "Start recording"}
                  </button>
                )}
                {state.shortcutAvailable !== false && ((status === "idle" && shortcutReady) || status === "recording") && <Shortcut />}
              </div>
            </div>
          </section>
        </div>

        <section className="panel output-panel" aria-labelledby="output-title">
          <div className="output-icon">
            {clipboardCopied ? <Check size={21} aria-hidden="true" /> : <FolderOpen size={21} aria-hidden="true" />}
          </div>
          <div className="output-copy">
            <h2 id="output-title">{recent ? "Latest recording" : "Recordings"}</h2>
            {recent ? (
              <p>
                <span className="output-filename" title={recent.path}>{recent.filename}</span>
                {clipboardCopied && <span className="copied-label" role="status">Copied to clipboard</span>}
              </p>
            ) : (
              <p>Finished files are saved to your Videos folder.</p>
            )}
          </div>
          <div className="output-actions">
            {recent && (
              <button type="button" className="plain-button" disabled={pending !== null || busy} onClick={() => void perform("copy", async () => {
                await api.copy();
                setCopiedPath(recent.path);
              }, "Couldn't copy the recording.")}>
                <Clipboard size={16} aria-hidden="true" />
                Copy file
              </button>
            )}
            <button
              type="button"
              className="plain-button"
              disabled={!recorder.ready || pending !== null || busy}
              onClick={() => void perform("folder", api.openFolder, "Couldn't open the Videos folder.")}
            >
              <FolderOpen size={16} aria-hidden="true" />
              Open folder
            </button>
          </div>
        </section>
      </main>
    </>
  );
}

export function RecorderController() {
  const { state } = useRecorder("controller");
  const elapsed = useElapsed(state.startedAt);
  const [waiting, setWaiting] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    if (state.status === "recording") {
      setWaiting(false);
      setError("");
    }
  }, [state.status, state.startedAt]);

  return (
    <main className="mini-controller">
      <div className="controller-content" data-tauri-drag-region>
        <span className="control-label" data-tauri-drag-region>
          <span className="status-dot" aria-hidden="true" />
          {error ? "Try again" : state.status === "processing" ? "Exporting" : "Recording"}
        </span>
        <time>{elapsed}</time>
      </div>
      <button
        type="button"
        className="mini-stop"
        aria-label="Stop recording"
        title={error || "Stop recording"}
        disabled={waiting || state.status !== "recording"}
        onClick={async () => {
          setWaiting(true);
          setError("");
          try {
            await api.stop();
          } catch (failure) {
            setError(errorText(failure, "Couldn't stop the recording."));
            setWaiting(false);
          }
        }}
      >
        <Square size={14} aria-hidden="true" fill="currentColor" />
        Stop
      </button>
    </main>
  );
}
