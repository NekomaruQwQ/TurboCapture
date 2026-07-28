// Unified server-events singleton.
//
// Subscribes to /api/events (one WebSocket) and fans the tagged JSON
// messages out to two reactive stores: `strings` and `kpm`.  Replaces the
// previous per-source endpoints (/api/strings/ws, /api/kpm) and their
// dedicated loops.
//
// Module-level singleton — lifetime = page lifetime.  The connection's
// AbortController is constructed but never aborted, so the loop runs
// forever.

import { runReconnectingWS } from "./ws";

/// All server-managed strings.  Keys may be user-provided (`marquee`,
/// `message`) or computed (`$liveProfile`, `$captureMode`, `$captureInfo`,
/// `$timestamp`).  Replaced wholesale on each server snapshot.
class StringsStore {
    value = $state<Record<string, string>>({});
}

/// Current keystrokes-per-minute.  `null` = encoder process not running
/// (server is publishing `{"kpm": null}`).
class KpmStore {
    value = $state<number | null>(null);
}

export const strings = new StringsStore();
export const kpm = new KpmStore();

interface KpmEvent {
    type: "kpm";
    kpm: number | null;
}
interface StringsEvent {
    type: "strings";
    data: Record<string, string>;
}
type EventMessage = KpmEvent | StringsEvent;

void runReconnectingWS("/api/events", new AbortController().signal,
    (ws) => new Promise<void>((resolve) => {
        ws.onmessage = (ev: MessageEvent) => {
            if (typeof ev.data !== "string") return;
            try {
                const msg = JSON.parse(ev.data) as EventMessage;
                switch (msg.type) {
                    case "kpm":     kpm.value = msg.kpm;        break;
                    case "strings": strings.value = msg.data;   break;
                }
            } catch (e) {
                console.error("events: failed to parse message:", e);
            }
        };
        ws.onclose = () => resolve();
        ws.onerror = () => resolve();
    }));
