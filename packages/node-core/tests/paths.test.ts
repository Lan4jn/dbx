import assert from "node:assert/strict";
import { test } from "vitest";
import { appDataDirFromInputs } from "../src/paths.js";

test("default app data dir is the home .drx directory on every platform", () => {
  assert.equal(appDataDirFromInputs({ platform: "linux", home: "/home/dbx" }), "/home/dbx/.drx");
  assert.equal(appDataDirFromInputs({ platform: "darwin", home: "/Users/dbx" }), "/Users/dbx/.drx");
  assert.equal(appDataDirFromInputs({ platform: "win32", home: "C:\\Users\\dbx" }), "C:\\Users\\dbx\\.drx");
});

test("DBX_DATA_DIR overrides the default app data dir", () => {
  assert.equal(appDataDirFromInputs({ platform: "linux", home: "/home/dbx", envDataDir: "/tmp/dbx-data" }), "/tmp/dbx-data");
});
