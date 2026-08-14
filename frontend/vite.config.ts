import { defineConfig } from "vite";

/** Returns the explicitly configured localhost development port. */
function developmentPort(command: string): number | undefined {
  if (command === "build") {
    return undefined;
  }

  const rawPort = process.env.LIVE_VITE_PORT;
  const port = Number(rawPort);
  if (rawPort === undefined || !Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new Error("LIVE_VITE_PORT must be an integer from 1 through 65535.");
  }
  return port;
}

export default defineConfig(({ command }) => ({
  server: {
    host: "127.0.0.1",
    port: developmentPort(command),
    strictPort: true,
  },
}));
