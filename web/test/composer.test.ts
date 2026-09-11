// The composer's max-iterations gate: only a positive integer reaches the
// run request; anything else omits the key (the server rejects 0).
import { describe, expect, test } from "bun:test";
import { parseMaxIterations } from "../src/components/composer";

describe("parseMaxIterations", () => {
  test("accepts a positive integer", () => {
    expect(parseMaxIterations("12")).toBe(12);
    expect(parseMaxIterations("1")).toBe(1);
  });

  test("omits blank, zero, and non-numeric input", () => {
    expect(parseMaxIterations("")).toBeUndefined();
    expect(parseMaxIterations("0")).toBeUndefined();
    expect(parseMaxIterations("abc")).toBeUndefined();
    expect(parseMaxIterations("-3")).toBeUndefined();
  });
});
