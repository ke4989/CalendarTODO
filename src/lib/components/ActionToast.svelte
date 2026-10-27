<script lang="ts">
  import ToastFrame from "$lib/components/ToastFrame.svelte";
  import { withoutTerminalPunctuation } from "$lib/i18n";
  let { message, variant = "error", stacked = false }: { message: string; variant?: "info" | "success" | "error"; stacked?: boolean } = $props();
  const visibleMessage = $derived(withoutTerminalPunctuation(message));
</script>

<ToastFrame {variant} {stacked}>{visibleMessage}</ToastFrame>
