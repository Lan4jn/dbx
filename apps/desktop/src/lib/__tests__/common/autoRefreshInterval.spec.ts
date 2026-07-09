import { describe, expect, it } from "vitest";
import { parseAutoRefreshIntervalSeconds } from "@/lib/common/autoRefreshInterval";

describe("parseAutoRefreshIntervalSeconds", () => {
  it("accepts positive integer seconds with surrounding whitespace", () => {
    expect(parseAutoRefreshIntervalSeconds(" 15 ")).toBe(15);
  });

  it("returns null when the prompt is cancelled or empty", () => {
    expect(parseAutoRefreshIntervalSeconds(null)).toBeNull();
    expect(parseAutoRefreshIntervalSeconds("   ")).toBeNull();
  });

  it("rejects invalid intervals", () => {
    expect(parseAutoRefreshIntervalSeconds("0")).toBeNull();
    expect(parseAutoRefreshIntervalSeconds("-1")).toBeNull();
    expect(parseAutoRefreshIntervalSeconds("1.5")).toBeNull();
    expect(parseAutoRefreshIntervalSeconds("abc")).toBeNull();
  });
});
