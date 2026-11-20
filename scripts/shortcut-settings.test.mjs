import assert from "node:assert/strict";
import test from "node:test";
import { createShortcutSettingsQueue } from "../src/shortcut-settings.ts";

function settings(id) {
  return { target: { kind: "monitor", id }, fps: 30, format: "mp4", autoCopy: true };
}

test("waits for one native write and replaces intermediate capture settings", async () => {
  const pending = Promise.withResolvers();
  const writes = [];
  const outcomes = [];
  const submit = createShortcutSettingsQueue((value) => {
    writes.push(value?.target.id ?? null);
    return writes.length === 1 ? pending.promise : Promise.resolve();
  }, (result) => outcomes.push(result));

  const first = submit("first", settings("first"));
  await submit("intermediate", settings("intermediate"));
  await submit("last", settings("last"));
  assert.deepEqual(writes, ["first"]);

  pending.resolve();
  await first;
  assert.deepEqual(writes, ["first", "last"]);
  assert.deepEqual(outcomes, [{ key: "last", status: "applied" }]);
});

test("disarms the shortcut when the selection disappears during a write", async () => {
  const pending = Promise.withResolvers();
  const writes = [];
  const outcomes = [];
  const submit = createShortcutSettingsQueue((value) => {
    writes.push(value);
    return writes.length === 1 ? pending.promise : Promise.resolve();
  }, (result) => outcomes.push(result));

  const monitor = settings("old-display");
  const first = submit("old-display", monitor);
  await submit("null", null);
  pending.reject(new Error("The display is gone"));
  await first;

  assert.deepEqual(writes, [monitor, null]);
  assert.deepEqual(outcomes, [{ key: "null", status: "applied" }]);
});

test("reports failed writes and accepts a retry for the same settings", async () => {
  const issue = new Error("IPC unavailable");
  const outcomes = [];
  let writes = 0;
  const submit = createShortcutSettingsQueue(async () => {
    if (++writes === 1) throw issue;
  }, (result) => outcomes.push(result));

  await submit("display", settings("display"));
  await submit("display", settings("display"));
  assert.deepEqual(outcomes, [{ key: "display", status: "failed", error: issue }, { key: "display", status: "applied" }]);
});
