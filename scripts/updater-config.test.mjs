import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("macOS updater uses the self-hosted release feed", async () => {
  const config = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  const runtimeSources = await Promise.all([
    readFile(new URL("../crates/dbx-core/src/update.rs", import.meta.url), "utf8"),
    readFile(new URL("../src-tauri/src/commands/update.rs", import.meta.url), "utf8"),
    readFile(new URL("../apps/desktop/src/composables/useAppUpdater.ts", import.meta.url), "utf8"),
  ]);

  assert.deepEqual(config.plugins.updater.endpoints, ["https://ser2.sjser.ccwu.cc:880/dbx/osx/latest.json"]);
  for (const source of runtimeSources) {
    assert.match(source, /https:\/\/ser2\.sjser\.ccwu\.cc:880\/dbx\/osx\//);
    assert.doesNotMatch(source, /server\.sjserver\.fun/);
  }
});
