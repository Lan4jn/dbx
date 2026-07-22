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
