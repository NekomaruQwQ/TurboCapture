import { describe, expect, test } from "bun:test";
import { parseViewerBinaryMessage, parseViewerControlMessage } from "./protocol";

describe("viewer control protocol", () => {
  test("parses render configuration into shader-ready names", () => {
    const message = parseViewerControlMessage(JSON.stringify({
      type: "render_configuration",
      configuration_generation: 7,
      profile: "desktop",
      render: {
        key_colors: [[0, 255, 0]],
        color_key_knee: { low: 0.02, high: 0.98 },
        binarization_color: [255, 255, 255],
      },
    }));

    expect(message).toEqual({
      type: "render_configuration",
      configuration: {
        configurationGeneration: 7,
        profile: "desktop",
        render: {
          keyColors: [[0, 255, 0]],
          kneeLow: 0.02,
          kneeHigh: 0.98,
          binarizationColor: [255, 255, 255],
        },
      },
    });
  });

  test("parses a structured server error", () => {
    expect(parseViewerControlMessage('{"type":"error","code":"capture","message":"stopped"}'))
      .toEqual({ type: "error", code: "capture", message: "stopped" });
  });

  test.each([
    "not-json",
    '{"type":"unknown"}',
    '{"type":"error","code":"capture"}',
    '{"type":"render_configuration","configuration_generation":1,"profile":null,"render":{"key_colors":[],"color_key_knee":{"low":1,"high":0},"binarization_color":null}}',
  ])("rejects malformed control input %s", (message) => {
    expect(() => parseViewerControlMessage(message)).toThrow();
  });
});

describe("viewer binary protocol", () => {
  test("builds a WebCodecs H.264 configuration from SPS and PPS", () => {
    const sps = new Uint8Array([0x67, 0x64, 0x00, 0x1f]);
    const pps = new Uint8Array([0x68, 0xee]);
    const payload = new Uint8Array(14 + sps.length + 2 + pps.length);
    const view = new DataView(payload.buffer);
    view.setBigUint64(0, 3n, true);
    view.setUint16(8, 1280, true);
    view.setUint16(10, 720, true);
    view.setUint16(12, sps.length, true);
    payload.set(sps, 14);
    view.setUint16(14 + sps.length, pps.length, true);
    payload.set(pps, 16 + sps.length);

    const message = parseViewerBinaryMessage(envelope(0x01, 0, payload));
    expect(message.kind).toBe("codec");
    if (message.kind !== "codec") {
      throw new Error("Expected codec configuration.");
    }
    expect(message.generation).toBe(3);
    expect(message.decoderConfig.codec).toBe("avc1.64001f");
    expect(message.decoderConfig.codedWidth).toBe(1280);
    expect(message.decoderConfig.codedHeight).toBe(720);
    expect([...new Uint8Array(message.decoderConfig.description as ArrayBuffer)]).toEqual([
      1, 0x64, 0x00, 0x1f, 0xff, 0xe1, 0, 4,
      ...sps,
      1, 0, 2,
      ...pps,
    ]);
  });

  test("parses generation, timestamp, key flag, and AVCC bytes", () => {
    const payload = new Uint8Array(22);
    const view = new DataView(payload.buffer);
    view.setBigUint64(0, 5n, true);
    view.setBigUint64(8, 123_456n, true);
    payload.set([0, 0, 0, 2, 0x65, 0x88], 16);

    const message = parseViewerBinaryMessage(envelope(0x02, 1, payload));
    expect(message).toEqual({
      kind: "access_unit",
      generation: 5,
      timestampUs: 123_456,
      keyframe: true,
      data: new Uint8Array([0, 0, 0, 2, 0x65, 0x88]),
    });
  });

  test("rejects mismatched envelope and malformed AVCC lengths", () => {
    const malformedEnvelope = new ArrayBuffer(8);
    new DataView(malformedEnvelope).setUint32(4, 1, true);
    expect(() => parseViewerBinaryMessage(malformedEnvelope)).toThrow("payload length");

    const payload = new Uint8Array(21);
    new DataView(payload.buffer).setUint32(16, 2, false);
    payload[20] = 0x65;
    expect(() => parseViewerBinaryMessage(envelope(0x02, 0, payload))).toThrow("NAL length");
  });
});

/** Wraps a test payload in the protocol's fixed eight-byte envelope. */
function envelope(type: number, flags: number, payload: Uint8Array): ArrayBuffer {
  const message = new Uint8Array(8 + payload.length);
  const view = new DataView(message.buffer);
  view.setUint8(0, type);
  view.setUint8(1, flags);
  view.setUint32(4, payload.length, true);
  message.set(payload, 8);
  return message.buffer;
}
