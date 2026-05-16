<script lang="ts">
    import Icon from "@/components/Icon.svelte";
    import LiveWidget from "./LiveWidget.svelte";

    type Props = {
        timeZone: string;
        label: string;
        variant?: "secondary";
    };

    let { timeZone, label, variant }: Props = $props();

    let format = $derived(new Intl.DateTimeFormat("en-GB", {
        timeZone,
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
    }));

    /// Initialized to "" — the effect below runs synchronously on mount and
    /// immediately writes the first formatted value, so no flash is visible.
    let time = $state("");

    /// Re-runs whenever `format` changes (i.e. when `timeZone` changes).
    $effect(() => {
        time = format.format(new Date());
        const id = setInterval(() => { time = format.format(new Date()); }, 60 * 1000);
        return () => clearInterval(id);
    });

    type ClockHour = 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12;
    let iconName = $derived.by(() => {
        const hours = (parseInt(time.split(":")[0] ?? "0", 10) % 12 || 12) as ClockHour;
        return `clock-${hours}` as const;
    });
</script>

<LiveWidget name={label} class={variant === "secondary" ? "opacity-50" : ""}>
    {#snippet icon()}
        <Icon name={iconName} size={40} />
    {/snippet}
    <span class="text-2xl">{time}</span>
</LiveWidget>
