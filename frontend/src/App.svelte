<script lang="ts">
    import StreamRenderer, { type StreamRendererProps } from "@/video/StreamRenderer.svelte";
    import { streamStatus } from "@/streams.svelte";
    import { strings } from "@/events.svelte";
    import Marquee from "@/components/Marquee.svelte";
    import Grid from "@/components/Grid.svelte";
    import LiveModeWidget from "@/widgets/LiveModeWidget.svelte";
    import ClaudeUsageWidget from "@/widgets/ClaudeUsageWidget.svelte";
    import AboutWidget from "@/widgets/AboutWidget.svelte";
    import KpmMeter from "@/KpmMeter.svelte";
    import ClockWidget from "@/widgets/ClockWidget.svelte";

    const liveMode = $derived(strings.value.$liveMode ?? "-");
    const appRendererProps: Partial<StreamRendererProps> = $derived.by(() => {
        if (liveMode === "code") {
            return {
                colorKey: ["#1d2129", "#282e3a"],
                colorKeyKnee: [0, 0.2],
            } as const;
        } else {
            return {};
        }
    });

    const youtubeMusicRendererProps: Partial<StreamRendererProps> = {
        colorKey: "#212121",
        colorKeyKnee: [0, 0.2],
        binarizationColor: "#ff8d46",
    };
</script>

<!--
    Pure viewer shell.  Stream lifecycle is fully server-managed — the
    frontend just renders two well-known stream IDs and polls for
    availability to show/hide the YouTube Music island.
-->
<Grid rows="1fr 60px" gap="2" class="w-screen h-screen p-2">
    <!-- Everything other than the YouTube Music island -->
    <Grid columns="1fr 3fr 40px" gap="2">
        <!-- Side Column: User Info -->
        <div class="flex! w-full h-full flex-col gap-2">
            <div class="island glow px-2 py-1.5">
                <Grid columns="1fr 1fr" gap="2">
                    <ClockWidget timeZone="Asia/Shanghai" label="UTC+8 Beijing" />
                    <ClockWidget timeZone="America/New_York" label="UTC-4 New York" variant="secondary" />
                </Grid>
            </div>
            <div class="island glow px-2 py-1.5">
                <LiveModeWidget />
            </div>
            <div class="island glow px-2 py-1.5">
                <ClaudeUsageWidget />
            </div>
            <div class="island glow px-3 py-2 flex-1">
                <pre class="font-sans font-light text-sm whitespace-pre-wrap wrap-break-word">{strings.value.message ?? ""}</pre>
            </div>
            <div class="island glow px-2 py-1.5">
                <AboutWidget />
            </div>
        </div>
        <!-- Main Column: Marquee + Main Stream -->
        <Grid rows="auto 1fr" gap="2">
            <!-- Top Row: Marquee Banner -->
            <div class="island glow overflow-clip">
                {#if strings.value.marquee}
                    <Marquee text={strings.value.marquee} />
                {/if}
            </div>
            <div
                id="main-stream"
                class={`island flex-col flex-1 rounded-md items-center justify-center ${(liveMode === "code" ? "glow" : " bg-[#1d1d1d]!")}`}>
                <StreamRenderer streamId="main" {...appRendererProps} />
            </div>
        </Grid>
        <!-- Side Column: Action Panel -->
        <div class="island glow p-2 flex! w-full h-full flex-col">
            <KpmMeter />
        </div>
    </Grid>
    <!-- Bottom Row: YouTube Music (conditionally rendered) -->
    <div class="island glow flex! items-center justify-center pt-1">
        {#if streamStatus.hasYouTubeMusic}
            <StreamRenderer streamId="youtube-music" {...youtubeMusicRendererProps} />
        {/if}
    </div>
</Grid>
