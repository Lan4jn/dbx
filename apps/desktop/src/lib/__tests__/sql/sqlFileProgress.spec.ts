import { describe, expect, it } from "vitest";
import { nextSqlFileProgressPercent, sqlFileCurrentStatement } from "@/lib/sql/sqlFileProgress";

describe("nextSqlFileProgressPercent", () => {
  it("uses byte progress while keeping running work below 100 percent", () => {
    expect(nextSqlFileProgressPercent(0, { status: "running", bytesProcessed: 25, totalBytes: 100 })).toBe(25);
    expect(nextSqlFileProgressPercent(0, { status: "running", bytesProcessed: 100, totalBytes: 100 })).toBe(99);
  });

  it("never moves backward within one execution", () => {
    expect(nextSqlFileProgressPercent(70, { status: "running", bytesProcessed: 50, totalBytes: 100 })).toBe(70);
  });

  it("sets successful terminal progress to 100 percent", () => {
    expect(nextSqlFileProgressPercent(99, { status: "done", bytesProcessed: 100, totalBytes: 100 })).toBe(100);
  });

  it("keeps a visible running fallback for legacy events without byte totals", () => {
    expect(nextSqlFileProgressPercent(0, { status: "running" })).toBe(8);
    expect(nextSqlFileProgressPercent(40, { status: "error" })).toBe(40);
  });
});

describe("sqlFileCurrentStatement", () => {
  it("prefers the bounded current statement and falls back to the summary", () => {
    expect(sqlFileCurrentStatement({ currentStatement: "SELECT 1", statementSummary: "SELECT" })).toBe("SELECT 1");
    expect(sqlFileCurrentStatement({ currentStatement: null, statementSummary: "SELECT" })).toBe("SELECT");
  });
});
