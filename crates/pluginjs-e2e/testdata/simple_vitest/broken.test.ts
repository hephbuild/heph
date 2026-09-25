import { describe, expect, it } from "vitest";

describe("broken", () => {
  it("fails on purpose", () => {
    expect(1 + 1).toBe(3);
  });
});
