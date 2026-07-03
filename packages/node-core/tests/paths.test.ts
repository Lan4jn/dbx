import assert from "node:assert/strict";
import { test } from "vitest";
import { appDataDirFromInputs } from "../src/paths.js";

test("default app data dir is the home .dbx directory on every platform", () => {
  assert.equal(appDataDirFromInputs({ platform: "linux", home: "/home/dbx" }), "/home/dbx/.dbx");
  assert.equal(appDataDirFromInputs({ platform: "darwin", home: "/Users/dbx" }), "/Users/dbx/.dbx");
  assert.equal(appDataDirFromInputs({ platform: "win32", home: "C:\\Users\\dbx" }), "C:\\Users\\dbx\\.dbx");
});

test("DBX_DATA_DIR overrides the default app data dir", () => {
  assert.equal(appDataDirFromInputs({ platform: "linux", home: "/home/dbx", envDataDir: "/tmp/dbx-data" }), "/tmp/dbx-data");
});
