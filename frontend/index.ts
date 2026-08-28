import { ColorKeyRenderer } from "./src/video/color-key";
import { CanvasViewer } from "./src/viewer";

const canvas = document.querySelector<HTMLCanvasElement>("#canvas");
if (canvas === null) {
  throw new Error("The canvas viewer requires a #canvas element.");
}

const renderer = new ColorKeyRenderer(canvas);
const viewer = new CanvasViewer(renderer);

/**
 * Applies the current hash route without reconnecting for render-only changes.
 * Invalid routes deliberately leave a transparent canvas instead of rendering error UI.
 */
function applyCanvasRoute(): void {
  try {
    viewer.applyRoute(window.location.hash);
  } catch (error) {
    console.error("Invalid TurboCapture canvas route.", error);
  }
}

window.addEventListener("hashchange", applyCanvasRoute);
window.addEventListener("pagehide", (event) => {
  viewer.stop();
  // A back/forward-cache entry must retain its GL resources for `pageshow`.
  if (!event.persisted) {
    renderer.dispose();
  }
});
window.addEventListener("pageshow", (event) => {
  if (event.persisted) {
    applyCanvasRoute();
  }
});
applyCanvasRoute();
