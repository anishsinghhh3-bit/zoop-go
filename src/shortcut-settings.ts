import type { Settings } from "./api";

export type ShortcutSettingsResult =
  | { key: string; status: "applied" }
  | { key: string; status: "failed"; error: unknown };

type ShortcutSettingsRequest = { key: string; settings: Settings | null };

export function createShortcutSettingsQueue(
  write: (settings: Settings | null) => Promise<unknown>,
  report: (result: ShortcutSettingsResult) => void,
) {
  let requested: ShortcutSettingsRequest | undefined;
  let writing = false;

  return async function submit(key: string, settings: Settings | null) {
    requested = { key, settings };
    if (writing) return;

    writing = true;
    try {
      while (requested) {
        const current: ShortcutSettingsRequest = requested;
        let result: ShortcutSettingsResult;
        try {
          await write(current.settings);
          result = { key: current.key, status: "applied" };
        } catch (error) {
          result = { key: current.key, status: "failed", error };
        }

        if (requested === current) {
          requested = undefined;
          report(result);
        }
      }
    } finally {
      writing = false;
    }
  };
}
