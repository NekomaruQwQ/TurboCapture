import { describe, expect, test } from "bun:test";
import { parseCanvasRoute } from "./route";

describe("parseCanvasRoute", () => {
  test("derives the localhost WebSocket URL from the approved hash route", () => {
    expect(parseCanvasRoute("#/canvas?port=48100")).toEqual({
      port: 48_100,
      websocketUrl: "ws://127.0.0.1:48100/api/video",
      render: {
        keyColors: [],
        kneeLow: 0.02,
        kneeHigh: 0.98,
        binarizationColor: undefined,
      },
    });
    expect(parseCanvasRoute("/canvas?port=1").port).toBe(1);
  });

  test("parses URL-owned key colors, knees, and binarization", () => {
    const route = parseCanvasRoute(
      "#/canvas?port=48100&key_colors=00ff00,01FE01&color_key_knee=0.01,0.20&binarization_color=ffffff",
    );
    expect(route.render).toEqual({
      keyColors: [[0, 255, 0], [1, 254, 1]],
      kneeLow: 0.01,
      kneeHigh: 0.20,
      binarizationColor: [255, 255, 255],
    });
  });

  test("accepts encoded separators and parameter reordering", () => {
    const route = parseCanvasRoute(
      "#/canvas?color_key_knee=0%2C1&key_colors=000000%2Cffffff&port=65535",
    );
    expect(route.render).toEqual({
      keyColors: [[0, 0, 0], [255, 255, 255]],
      kneeLow: 0,
      kneeHigh: 1,
      binarizationColor: undefined,
    });
  });

  test("accepts the shader's eight-color limit", () => {
    const route = parseCanvasRoute(`#/canvas?port=1&key_colors=${Array(8).fill("abcdef").join(",")}`);
    expect(route.render.keyColors).toEqual(Array(8).fill([171, 205, 239]));
  });

  test("accepts decimal exponents for finite knees", () => {
    expect(parseCanvasRoute("#/canvas?port=1&color_key_knee=1e-2,.2").render).toMatchObject({
      kneeLow: 0.01,
      kneeHigh: 0.2,
    });
  });

  test.each([
    "key_colors=",
    "key_colors=fff",
    "key_colors=%2300ff00",
    "key_colors=00ff00ff",
    "key_colors=ggff00",
    "key_colors=00ff00%0A",
    "key_colors=00ff00,",
    "key_colors=00ff00,,ffffff",
    `key_colors=${Array(9).fill("00ff00").join(",")}`,
    "key_colors=00ff00&key_colors=ffffff",
    "color_key_knee=",
    "color_key_knee=,1",
    "color_key_knee=0,",
    "color_key_knee=0,0.5,1",
    "color_key_knee=0.5,0.5",
    "color_key_knee=0.8,0.2",
    "color_key_knee=-0.1,1",
    "color_key_knee=0,1.1",
    "color_key_knee=0,NaN",
    "color_key_knee=0,Infinity",
    "color_key_knee=0,1e999",
    "color_key_knee=0.5,0.5000000001",
    "color_key_knee=0,1%0A",
    "color_key_knee=0x0,1",
    "color_key_knee=%20,1",
    "color_key_knee=0,1&color_key_knee=0.1,0.2",
    "binarization_color=",
    "binarization_color=ffffff00",
    "binarization_color=ffffff%0A",
    "binarization_color=ffffff,000000",
    "binarization_color=ffffff&binarization_color=000000",
  ])("rejects malformed render parameters %s", (query) => {
    expect(() => parseCanvasRoute(`#/canvas?port=48100&${query}`)).toThrow();
  });

  test.each([
    "",
    "#/",
    "#/canvas",
    "#/canvas?port=0",
    "#/canvas?port=65536",
    "#/canvas?port=1%0A",
    "#/canvas?port=48100&port=48101",
    "#/canvas?port=48100&host=example.com",
    "#/viewer?port=48100",
  ])("rejects malformed or expanded route %s", (route) => {
    expect(() => parseCanvasRoute(route)).toThrow();
  });
});
