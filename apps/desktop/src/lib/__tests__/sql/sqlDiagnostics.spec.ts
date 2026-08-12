import { describe, expect, it } from "vitest";
import { buildSqlErrorPresentation, sqlErrorDocumentRange } from "@/lib/sql/sqlDiagnostics";

describe("buildSqlErrorPresentation", () => {
  it("resolves one-based line and column locations", () => {
    expect(buildSqlErrorPresentation("syntax error at line 2, column 4", "select 1;\nselect x")).toEqual({
      line: 1,
      column: 3,
      offset: 13,
      lineText: "select x",
      caretColumn: 3,
    });
  });

  it("uses the start of a reported line when no column is available", () => {
    expect(buildSqlErrorPresentation("parse failed at line 3", "one\ntwo\nthree")).toMatchObject({
      line: 2,
      column: 0,
      offset: 8,
      lineText: "three",
      caretColumn: 0,
    });
  });

  it.each([
    ["Position: 12", 11],
    ["syntax error at character 8", 7],
  ])("converts absolute one-based positions from %s", (message, offset) => {
    expect(buildSqlErrorPresentation(message, "select 1;\nselect x")).toMatchObject({ offset });
  });

  it("aligns PostgreSQL caret messages to the SQL text after the LINE prefix", () => {
    const message = "ERROR: syntax error\nLINE 2: select fro users\n               ^";
    expect(buildSqlErrorPresentation(message, "select 1;\nselect fro users")).toMatchObject({
      line: 1,
      column: 7,
      lineText: "select fro users",
      caretColumn: 7,
    });
  });

  it("clamps reported columns and positions to the SQL bounds", () => {
    expect(buildSqlErrorPresentation("line 2, column 99", "one\ntwo")).toMatchObject({ line: 1, column: 3, offset: 7 });
    expect(buildSqlErrorPresentation("Position: 999", "select 1")).toMatchObject({ line: 0, column: 8, offset: 8 });
  });

  it("returns null for unrecognized errors or invalid lines", () => {
    expect(buildSqlErrorPresentation("syntax error near FROM", "select 1")).toBeNull();
    expect(buildSqlErrorPresentation("syntax error at line 9", "select 1")).toBeNull();
  });

  it("bounds long SQL lines while keeping the caret visible", () => {
    const sql = `${"a".repeat(220)} broken_token ${"z".repeat(220)}`;
    const presentation = buildSqlErrorPresentation("line 1, column 225", sql);

    expect(presentation?.lineText.length).toBeLessThanOrEqual(166);
    expect(presentation?.lineText).toContain("broken_token");
    expect(presentation?.lineText[presentation.caretColumn]).toBe("k");
  });
});

describe("sqlErrorDocumentRange", () => {
  it("maps a statement-local error offset into the editor document", () => {
    const sql = "select 1;\nselect fro users;";
    const sourceFrom = sql.indexOf("select fro");
    const presentation = buildSqlErrorPresentation("line 1, column 8", "select fro users;");

    expect(sqlErrorDocumentRange(presentation, { from: sourceFrom, to: sql.length })).toEqual({
      from: sourceFrom + 7,
      to: sourceFrom + 8,
    });
  });

  it("returns null when source metadata is unavailable", () => {
    const presentation = buildSqlErrorPresentation("line 1, column 1", "select 1");
    expect(sqlErrorDocumentRange(presentation, undefined)).toBeNull();
  });
});
