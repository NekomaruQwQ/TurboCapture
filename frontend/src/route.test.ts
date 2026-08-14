import { describe, expect, test } from "bun:test";
import { parseCanvasRoute } from "./route";

describe("parseCanvasRoute", () => {
  test("derives the localhost WebSocket URL from the approved hash route", () => {
    expect(parseCanvasRoute("#/canvas?port=48100")).toEqual({
      port: 48_100,
      websocketUrl: "ws://127.0.0.1:48100/api/video",
    });
    expect(parseCanvasRoute("/canvas?port=1").port).toBe(1);
  });

  test.each([
    "",
    "#/",
    "#/canvas",
    "#/canvas?port=0",
    "#/canvas?port=65536",
    "#/canvas?port=48100&port=48101",
    "#/canvas?port=48100&host=example.com",
    "#/viewer?port=48100",
  ])("rejects malformed or expanded route %s", (route) => {
    expect(() => parseCanvasRoute(route)).toThrow();
  });
});
