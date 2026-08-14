/** The validated client-side route for one localhost capture instance. */
export interface CanvasRoute {
  /** Capture server port forwarded onto the viewing machine. */
  readonly port: number;
  /** WebSocket endpoint derived exclusively from the validated port. */
  readonly websocketUrl: string;
}

/**
 * Parses the exact `#/canvas?port=<port>` route accepted by the viewer.
 * Throws when the path, query shape, or TCP port is invalid.
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
  const keys = [...parameters.keys()];
  if (keys.length !== 1 || keys[0] !== "port") {
    throw new Error("The viewer route requires exactly one port parameter.");
  }

  const rawPort = parameters.get("port");
  if (rawPort === null || !/^\d{1,5}$/.test(rawPort)) {
    throw new Error("The capture port must contain one to five decimal digits.");
  }
  const port = Number(rawPort);
  if (port < 1 || port > 65_535) {
    throw new Error("The capture port must be from 1 through 65535.");
  }

  return {
    port,
    websocketUrl: `ws://127.0.0.1:${port}/api/video`,
  };
}
