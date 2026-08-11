import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("macOS updater uses the self-hosted release feed", async () => {
  const config = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));

  assert.deepEqual(config.plugins.updater.endpoints, ["https://server.sjserver.fun:880/dbx/osx/latest.json"]);
});
