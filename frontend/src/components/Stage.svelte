<script lang="ts">
    import { onMount, type Snippet } from "svelte";
    import {
        STAGE_HEIGHT,
        STAGE_WIDTH,
        fitStage,
    } from "./stage";

    type Props = {
        children: Snippet;
    };

    let { children }: Props = $props();

    let viewport: HTMLDivElement;
    let layout = $state(fitStage(0, 0));

    /// Observe the component's actual host box rather than global window state
    /// so the stage remains correct if it is embedded in another container.
    onMount(() => {
        layout = fitStage(
            viewport.clientWidth,
            viewport.clientHeight,
        );

        const observer = new ResizeObserver(([entry]) => {
            if (!entry) return;
            layout = fitStage(
                entry.contentRect.width,
                entry.contentRect.height,
            );
        });
        observer.observe(viewport);

        return () => observer.disconnect();
    });
</script>

<!--
    LiveUI is authored in one stable 1280×720 coordinate system, but its hosts
    expose that design differently: live-app can use device scale for a denser
    raster, while OBS presents a larger CSS viewport directly. Stage contains
    that host-specific difference here so every child keeps the same geometry
    and remains mounted while the host is resized.
-->
<div bind:this={viewport} class="stage-viewport">
    <div
        class="stage"
        style:width={`${STAGE_WIDTH}px`}
        style:height={`${STAGE_HEIGHT}px`}
        style:left={`${layout.left}px`}
        style:top={`${layout.top}px`}
        style:transform={`scale(${layout.scale})`}>
        {@render children()}
    </div>
</div>

<style>
    .stage-viewport {
        position: relative;
        width: 100vw;
        height: 100vh;
        overflow: hidden;
    }

    /* Explicit offsets plus a top-left origin make letterboxing independent
       from transformed layout bounds, which CSS does not include in centering. */
    .stage {
        position: absolute;
        transform-origin: top left;
        background: url("/img/background.png") center / cover no-repeat, #1a1b1e;
    }
</style>
