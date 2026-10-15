import type { SqlFileProgress, SqlFileStatus } from "@/lib/backend/tauri";

type SqlFileProgressBytes = Pick<SqlFileProgress, "bytesProcessed" | "totalBytes"> & {
  status: SqlFileStatus;
};

export function nextSqlFileProgressPercent(previous: number, progress: SqlFileProgressBytes): number {
  if (progress.status === "done") return 100;

  const processed = progress.bytesProcessed;
  const total = progress.totalBytes;
  if (typeof processed === "number" && typeof total === "number" && Number.isFinite(processed) && Number.isFinite(total) && total > 0) {
    const bytePercent = Math.min(99, Math.max(0, Math.round((Math.max(0, processed) / total) * 100)));
    return Math.max(previous, bytePercent);
  }

  if (progress.status === "started" || progress.status === "running" || progress.status === "statementDone" || progress.status === "statementFailed") {
    return Math.max(previous, 8);
  }
  return previous;
}

export function sqlFileCurrentStatement(progress: Pick<SqlFileProgress, "currentStatement" | "statementSummary"> | null | undefined): string {
  return progress?.currentStatement?.trim() || progress?.statementSummary || "";
}

export function sqlFileProgressPercent(status: string, bytesRead?: number, totalBytes?: number): number | null {
  if (status.toLowerCase() === "done") return 100;
  if (bytesRead === undefined || totalBytes === undefined || !Number.isFinite(bytesRead) || !Number.isFinite(totalBytes) || bytesRead < 0 || totalBytes <= 0) return null;
  return Math.min(99, Math.floor((bytesRead / totalBytes) * 100));
}

export function formatSqlFileBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Math.max(0, bytes);
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${unitIndex === 0 ? value : value.toFixed(1)} ${units[unitIndex]}`;
}
