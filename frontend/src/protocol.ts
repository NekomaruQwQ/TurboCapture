/** Target availability sent independently of video data and URL-owned presentation. */
export interface StreamState {
  /** Selected profile name, or null while no capture profile is active. */
  readonly profile: string | null;
}

/** Structured server failure sent before the connection closes. */
export interface ViewerError {
  readonly type: "error";
  readonly code: string;
  readonly message: string;
}

/** Validated text messages accepted from the viewer WebSocket. */
export type ViewerControlMessage =
  | { readonly type: "stream_state"; readonly state: StreamState }
  | ViewerError;

/** Validated H.264 decoder configuration carried by a binary envelope. */
export interface CodecConfiguration {
  readonly kind: "codec";
  /** Generation shared by the configuration and its access units. */
  readonly generation: number;
  /** Complete configuration accepted directly by WebCodecs. */
  readonly decoderConfig: VideoDecoderConfig;
}

/** Validated AVCC access unit carried by a binary envelope. */
export interface AccessUnit {
  readonly kind: "access_unit";
  /** Codec generation required to decode this unit. */
  readonly generation: number;
  /** Presentation timestamp in microseconds. */
  readonly timestampUs: number;
  /** Whether the access unit can begin a new decode sequence. */
  readonly keyframe: boolean;
  /** Complete AVCC payload accepted directly by WebCodecs. */
  readonly data: Uint8Array;
}

/** Validated binary messages accepted from the viewer WebSocket. */
export type ViewerBinaryMessage = CodecConfiguration | AccessUnit;

const HEADER_SIZE = 8;
const CODEC_MESSAGE = 0x01;
const ACCESS_UNIT_MESSAGE = 0x02;
const KEYFRAME_FLAG = 1 << 0;
const MAX_PAYLOAD_SIZE = 16 * 1024 * 1024;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);

/** Parses and validates a JSON control message from the capture server. */
export function parseViewerControlMessage(text: string): ViewerControlMessage {
  let value: unknown;
  try {
    value = JSON.parse(text) as unknown;
  } catch (error) {
    throw new Error("Viewer control message is not valid JSON.", { cause: error });
  }

  const message = requireObject(value, "viewer control message");
  const type = requireString(message.type, "viewer control message type");
  if (type === "error") {
    requireExactKeys(message, ["type", "code", "message"], "error message");
    return {
      type,
      code: requireString(message.code, "error code"),
      message: requireString(message.message, "error message"),
    };
  }
  if (type !== "stream_state") {
    throw new Error(`Unsupported viewer control message type: ${type}.`);
  }

  requireExactKeys(message, ["type", "profile"], "stream-state message");
  const profile = message.profile;
  return {
    type,
    state: { profile: profile === null ? null : requireString(profile, "profile") },
  };
}

/** Parses and validates one complete binary protocol envelope. */
export function parseViewerBinaryMessage(buffer: ArrayBuffer): ViewerBinaryMessage {
  const bytes = new Uint8Array(buffer);
  if (bytes.byteLength < HEADER_SIZE) {
    throw new Error("Viewer binary message is shorter than its envelope header.");
  }

  const view = new DataView(buffer);
  const messageType = view.getUint8(0);
  const flags = view.getUint8(1);
  if (view.getUint16(2, true) !== 0) {
    throw new Error("Viewer binary message reserved bits must be zero.");
  }
  const payloadLength = view.getUint32(4, true);
  if (payloadLength > MAX_PAYLOAD_SIZE) {
    throw new Error("Viewer binary message exceeds the 16 MiB payload limit.");
  }
  if (payloadLength !== bytes.byteLength - HEADER_SIZE) {
    throw new Error("Viewer binary message payload length does not match its envelope.");
  }
  const payload = bytes.subarray(HEADER_SIZE);

  if (messageType === CODEC_MESSAGE) {
    if (flags !== 0) {
      throw new Error("Codec-configuration messages cannot carry flags.");
    }
    return parseCodecConfiguration(payload);
  }
  if (messageType === ACCESS_UNIT_MESSAGE) {
    if ((flags & ~KEYFRAME_FLAG) !== 0) {
      throw new Error("Access-unit message contains unsupported flags.");
    }
    return parseAccessUnit(payload, (flags & KEYFRAME_FLAG) !== 0);
  }
  throw new Error(`Unsupported viewer binary message type: ${messageType}.`);
}

/** Validates and converts the codec payload into a WebCodecs configuration. */
function parseCodecConfiguration(payload: Uint8Array): CodecConfiguration {
  if (payload.byteLength < 18) {
    throw new Error("Codec-configuration payload is too short.");
  }
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const generation = readSafeUint64(view, 0, "codec generation");
  const width = view.getUint16(8, true);
  const height = view.getUint16(10, true);
  if (width === 0 || height === 0) {
    throw new Error("Codec dimensions must be non-zero.");
  }

  const spsLength = view.getUint16(12, true);
  const spsStart = 14;
  const ppsLengthOffset = spsStart + spsLength;
  if (spsLength < 4 || ppsLengthOffset + 2 > payload.byteLength) {
    throw new Error("Codec payload contains an invalid SPS length.");
  }
  const ppsLength = view.getUint16(ppsLengthOffset, true);
  const ppsStart = ppsLengthOffset + 2;
  if (ppsLength === 0 || ppsStart + ppsLength !== payload.byteLength) {
    throw new Error("Codec payload contains an invalid PPS length.");
  }

  const sps = payload.subarray(spsStart, ppsLengthOffset);
  const pps = payload.subarray(ppsStart);
  const codec = `avc1.${toHex(sps[1])}${toHex(sps[2])}${toHex(sps[3])}`;
  return {
    kind: "codec",
    generation,
    decoderConfig: {
      codec,
      codedWidth: width,
      codedHeight: height,
      description: makeAvcConfigurationRecord(sps, pps),
    },
  };
}

/** Validates the access-unit metadata and its internal AVCC NAL framing. */
function parseAccessUnit(payload: Uint8Array, keyframe: boolean): AccessUnit {
  if (payload.byteLength < 20) {
    throw new Error("Access-unit payload is too short.");
  }
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const data = payload.subarray(16);
  validateAvcc(data);
  return {
    kind: "access_unit",
    generation: readSafeUint64(view, 0, "access-unit generation"),
    timestampUs: readSafeUint64(view, 8, "access-unit timestamp"),
    keyframe,
    data,
  };
}

/** Creates the AVCDecoderConfigurationRecord required for AVCC input. */
function makeAvcConfigurationRecord(sps: Uint8Array, pps: Uint8Array): Uint8Array {
  const result = new Uint8Array(11 + sps.byteLength + pps.byteLength);
  const view = new DataView(result.buffer);
  result.set([1, sps[1] ?? 0, sps[2] ?? 0, sps[3] ?? 0, 0xff, 0xe1], 0);
  view.setUint16(6, sps.byteLength, false);
  result.set(sps, 8);
  const ppsCountOffset = 8 + sps.byteLength;
  result[ppsCountOffset] = 1;
  view.setUint16(ppsCountOffset + 1, pps.byteLength, false);
  result.set(pps, ppsCountOffset + 3);
  return result;
}

/** Rejects empty, truncated, or trailing data in an AVCC access unit. */
function validateAvcc(data: Uint8Array): void {
  if (data.byteLength < 5) {
    throw new Error("AVCC access unit must contain at least one non-empty NAL unit.");
  }
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let offset = 0;
  while (offset < data.byteLength) {
    if (offset + 4 > data.byteLength) {
      throw new Error("AVCC access unit ends inside a NAL length prefix.");
    }
    const nalLength = view.getUint32(offset, false);
    offset += 4;
    if (nalLength === 0 || offset + nalLength > data.byteLength) {
      throw new Error("AVCC access unit contains an invalid NAL length.");
    }
    offset += nalLength;
  }
}

/** Reads a u64 that JavaScript can represent without losing precision. */
function readSafeUint64(view: DataView, offset: number, name: string): number {
  const value = view.getBigUint64(offset, true);
  if (value > MAX_SAFE_BIGINT) {
    throw new Error(`${name} exceeds JavaScript's safe integer range.`);
  }
  return Number(value);
}

/** Requires a plain JSON object rather than null, an array, or a primitive. */
function requireObject(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${name} must be an object.`);
  }
  return value as Record<string, unknown>;
}

/** Requires an object to contain exactly the private-protocol fields named. */
function requireExactKeys(
  value: Record<string, unknown>,
  expectedKeys: readonly string[],
  name: string,
): void {
  const actualKeys = Object.keys(value).sort();
  const expected = [...expectedKeys].sort();
  if (actualKeys.length !== expected.length || actualKeys.some((key, index) => key !== expected[index])) {
    throw new Error(`${name} contains missing or unsupported fields.`);
  }
}

/** Requires a non-empty JSON string. */
function requireString(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be a non-empty string.`);
  }
  return value;
}

/** Formats one byte as exactly two lowercase hexadecimal digits. */
function toHex(value: number | undefined): string {
  if (value === undefined) {
    throw new Error("SPS is missing codec identification bytes.");
  }
  return value.toString(16).padStart(2, "0");
}
