<script lang="ts">
    import Icon from "@/components/Icon.svelte";
    import LiveWidget from "./LiveWidget.svelte";
    import { strings } from "@/events.svelte";

    /// Display labels and icons for each capture profile.
    const PROFILE_MAP = {
        unknown: { label: "—", icon: "activity" },
        code: { label: "Coding", icon: "bug" },
        game: { label: "Gaming", icon: "gamepad" },
        sing: { label: "Singing", icon: "music" },
        chat: { label: "Chatting", icon: "message-circle" },
        brb: { label: "BRB", icon: "coffee" },
    } as const;

    let profile = $derived(
        (strings.value.$liveProfile
            && PROFILE_MAP[strings.value.$liveProfile as keyof typeof PROFILE_MAP])
            || PROFILE_MAP.unknown);
    let captureMode = $derived(strings.value.$captureMode?.toUpperCase() ?? "UNKNOWN");
    let captureInfo = $derived(strings.value.$captureInfo ?? "");
</script>

<!-- Shows the presentation associated with the selected window's capture profile. -->
<LiveWidget name={`Live Capture - ${captureMode}`}>
    {#snippet icon()}
        <Icon name={profile.icon} size={40} />
    {/snippet}
    <span class="text-lg">{profile.label} - {captureInfo}</span>
</LiveWidget>
