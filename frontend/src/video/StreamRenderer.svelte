<script lang="ts" module>
    export type StreamRendererProps = {
        streamId: string;
        /// Hex color (e.g. "#212121") or list of hex colors to key out.
        /// When omitted or empty the shader degenerates to a sRGB-correct
        /// passthrough — same WebGL2 path either way, so the canvas's
        /// context type stays stable across prop changes.
        colorKey?: string | string[];
        /// Smoothstep knee `[low, high]` over the unspill ratio in [0,1].
        /// `low` is the noise floor (≤ low → fully transparent); `high` is
        /// the solid snap (≥ high → fully opaque).  Falls back to the
        /// renderer's defaults (≈ 0.02 / 0.98) when unset.
        colorKeyKnee?: [number, number];
        /// Hex sRGB color (e.g. "#FF00FF") that replaces the kept-pixel RGB
        /// while preserving the keyer's soft alpha — useful for solid-color
        /// silhouettes.  Without a `colorKey` the entire frame is opaque and
        /// becomes a flat fill of this color, which is rarely what you want.
        binarizationColor?: string;
    };
</script>

<script lang="ts">
    import { untrack } from "svelte";
    import { ColorKeyRenderer, parseHexColor } from "./color-key";
    import { startStreamLoop } from "./stream-loop";

    let { streamId, colorKey, colorKeyKnee, binarizationColor }: StreamRendererProps = $props();

    let canvas: HTMLCanvasElement;
    let renderer: ColorKeyRenderer | null = $state(null);

    /// Normalise the prop to a plain array so the rest of the component only
    /// has one shape to think about.  Returns `[]` when nothing is keyed out.
    const keyList = $derived(
        colorKey === undefined ? []
            : typeof colorKey === "string" ? [colorKey]
            : colorKey);

    /// Lifecycle: own the WebGL2 context, the ColorKeyRenderer, and the
    /// stream loop (which in turn owns the VideoDecoder + WebSocket).
    /// Reads `streamId` via `untrack` because Svelte's props proxy
    /// invalidates on every parent re-render — without untrack, any prop
    /// change anywhere (e.g. flipping `colorKey` from the parent's spread)
    /// would tear the decoder + WS down even though `streamId` itself
    /// never changes value.  `canvas` is a plain `let` (non-reactive in
    /// runes mode) populated by `bind:this`, so it isn't tracked either.
    /// Net effect: this runs exactly once on mount.
    $effect(() => {
        if (!canvas) {
            console.error("StreamRenderer: Canvas ref is null!");
            return;
        }
        const id = untrack(() => streamId);

        // Always go through WebGL2 — even with no keys the shader is a
        // passthrough.  Mixing 2D and WebGL2 contexts on the same canvas
        // node is impossible (a canvas binds to exactly one context kind
        // for its DOM lifetime), so we commit to WebGL2 up front.
        const r = new ColorKeyRenderer(canvas);
        renderer = r;
        console.log("StreamRenderer: Using WebGL color-key renderer for streamId=%s", id);

        const abortController = new AbortController();
        void startStreamLoop(id, (frame) => r.render(frame), abortController.signal);

        return () => {
            console.log("StreamRenderer: Component unmounting, aborting stream loop");
            abortController.abort();
            r.dispose();
            renderer = null;
        };
    });

    /// Push color-key params as uniforms whenever they change.  Runs once
    /// after the lifecycle effect creates the renderer (because `renderer`
    /// flips from null → instance is itself a tracked dep), then on every
    /// subsequent prop flip.  Never touches the decoder or stream loop.
    $effect(() => {
        if (!renderer) return;
        renderer.updateParams({
            keyColors: keyList.map(parseHexColor),
            kneeLow: colorKeyKnee?.[0],
            kneeHigh: colorKeyKnee?.[1],
            binarizationColor: binarizationColor ? parseHexColor(binarizationColor) : undefined,
        });
    });
</script>

<canvas
    bind:this={canvas}
    class="w-full object-contain">
</canvas>
