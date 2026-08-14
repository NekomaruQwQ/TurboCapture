import { describe, expect, test } from "bun:test";
import { nextReconnectDelay } from "./ws";

describe("nextReconnectDelay", () => {
  test("doubles failed attempts and caps the delay at five seconds", () => {
    expect(nextReconnectDelay(100, false)).toBe(200);
    expect(nextReconnectDelay(3_200, false)).toBe(5_000);
    expect(nextReconnectDelay(5_000, false)).toBe(5_000);
  });

  test("resets only after a decoded frame marks the connection healthy", () => {
    expect(nextReconnectDelay(5_000, true)).toBe(100);
  });
});
