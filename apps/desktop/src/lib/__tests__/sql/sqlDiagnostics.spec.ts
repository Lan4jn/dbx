import { describe, expect, it } from "vitest";
import { buildSqlErrorPresentation } from "@/lib/sql/sqlDiagnostics";

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
});
