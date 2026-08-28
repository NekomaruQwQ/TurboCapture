import { DEFAULT_KNEE_HIGH, DEFAULT_KNEE_LOW, MAX_KEYS, type ColorKeyParams } from "./video/color-key";

/** The validated client-side route for one localhost capture instance. */
export interface CanvasRoute {
  /** Capture server port forwarded onto the viewing machine. */
  readonly port: number;
  /** WebSocket endpoint derived exclusively from the validated port. */
  readonly websocketUrl: string;
  /** Complete presentation settings owned by this viewer, independent of the server. */
  readonly render: ColorKeyParams;
}

/**
 * Parses `#/canvas?port=<port>` with optional color-key parameters.
 * Omitted render fields reset to passthrough, default knees, and no binarization.
 * Throws for unknown/duplicate parameters, invalid colors or knees, or an invalid port/path.
 */
export function parseCanvasRoute(hash: string): CanvasRoute {
  const route = hash.startsWith("#") ? hash.slice(1) : hash;
  const queryIndex = route.indexOf("?");
  const path = queryIndex === -1 ? route : route.slice(0, queryIndex);
  const query = queryIndex === -1 ? "" : route.slice(queryIndex + 1);
  if (path !== "/canvas") {
    throw new Error("The viewer route must be /canvas.");
  }

  const parameters = new URLSearchParams(query);
  const seen = new Set<string>();
  for (const key of parameters.keys()) {
    if (!["port", "key_colors", "color_key_knee", "binarization_color"].includes(key)) {
      throw new Error(`Unknown viewer parameter: ${key}.`);
    }
    if (seen.has(key)) {
      throw new Error(`Duplicate viewer parameter: ${key}.`);
    }
    seen.add(key);
  }

  const rawPort = parameters.get("port");
  if (rawPort === null || rawPort.trim() !== rawPort || !/^\d{1,5}$/.test(rawPort)) {
    throw new Error("The capture port must contain one to five decimal digits.");
  }
  const port = Number(rawPort);
  if (port < 1 || port > 65_535) {
    throw new Error("The capture port must be from 1 through 65535.");
  }

  const rawColors = parameters.get("key_colors");
  const colors = rawColors === null ? [] : rawColors.split(",");
  if (colors.length > MAX_KEYS) {
    throw new Error(`key_colors supports at most ${MAX_KEYS} colors.`);
  }
  const rawKnee = parameters.get("color_key_knee");
  const [kneeLow, kneeHigh] = rawKnee === null
    ? [DEFAULT_KNEE_LOW, DEFAULT_KNEE_HIGH]
    : parseKnee(rawKnee);
  const rawBinarization = parameters.get("binarization_color");

  return {
    port,
    websocketUrl: `ws://127.0.0.1:${port}/api/video`,
    render: {
      keyColors: colors.map((color) => parseRgb(color, "key_colors")),
      kneeLow,
      kneeHigh,
      binarizationColor: rawBinarization === null
        ? undefined
        : parseRgb(rawBinarization, "binarization_color"),
    },
  };
}

/** Parses one six-digit sRGB hex color; prefixes, alpha, and shorthand are rejected. */
function parseRgb(value: string, name: string): [number, number, number] {
  if (value.length !== 6 || !/^[\da-f]{6}$/i.test(value)) {
    throw new Error(`${name} must contain six-digit RGB hex colors without a # prefix.`);
  }
  return [
    Number.parseInt(value.slice(0, 2), 16),
    Number.parseInt(value.slice(2, 4), 16),
    Number.parseInt(value.slice(4, 6), 16),
  ];
}

/** Parses two decimal knees, rejecting coercions such as empty strings, whitespace, and hex. */
function parseKnee(value: string): [number, number] {
  const parts = value.split(",");
  if (parts.length !== 2 || parts.some((part) =>
    part.trim() !== part || !/^(?:\d+(?:\.\d+)?|\.\d+)(?:e[+-]?\d+)?$/i.test(part))) {
    throw new Error("color_key_knee must contain two comma-separated decimal numbers.");
  }
  const low = Number(parts[0]);
  const high = Number(parts[1]);
  if (!Number.isFinite(low) || !Number.isFinite(high) || low < 0 || high > 1 || low >= high) {
    throw new Error("color_key_knee must satisfy 0 <= low < high <= 1.");
  }
  // Distinct JS doubles can collapse to equal shader floats, making smoothstep undefined.
  if (Math.fround(low) >= Math.fround(high)) {
    throw new Error("color_key_knee edges must remain distinct at shader float precision.");
  }
  return [low, high];
}
