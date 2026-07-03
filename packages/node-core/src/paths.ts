import { homedir, platform } from "node:os";
import { join, posix, win32 } from "node:path";

export function appDataDir(): string {
  // 支持 DBX_DATA_DIR 环境变量（与 Rust 侧 data_dir.rs 保持一致）
  return appDataDirFromInputs({
    platform: platform(),
    home: homedir(),
    appData: process.env.APPDATA,
    envDataDir: process.env.DBX_DATA_DIR,
  });
}

export function appDataDirFromInputs(options: { platform: NodeJS.Platform; home: string; appData?: string; envDataDir?: string }): string {
  if (options.envDataDir && options.envDataDir.trim() !== "") {
    return options.envDataDir;
  }

  return options.platform === "win32" ? win32.join(options.home, ".dbx") : posix.join(options.home, ".dbx");
}

export function dbPath(): string {
  return join(appDataDir(), "dbx.db");
}

export function bridgePortFilePath(): string {
  return join(appDataDir(), "mcp-bridge-port");
}
