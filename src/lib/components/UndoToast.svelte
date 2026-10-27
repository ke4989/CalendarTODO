<script lang="ts">
  import Button from "$lib/components/Button.svelte";
  import ToastFrame from "$lib/components/ToastFrame.svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  let { title, undo, language = "zh-CN", stacked = false }: { title: string; undo: () => void; language?: ResolvedLanguage; stacked?: boolean } = $props();
</script>

<ToastFrame variant="success" duration="undo" {stacked}>
  {translate(language, "deleted")} “{title}”
  {#snippet action()}
    <Button variant="quiet" size="compact" type="button" onclick={undo} aria-label={`${translate(language, "undoDelete")} ${title}`}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6-5 5 5 5M4 11h10a6 6 0 1 1 0 12" /></svg>
      {translate(language, "undo")}
    </Button>
  {/snippet}
</ToastFrame>

<style>
  :global(.toast-frame .ui-button.quiet){min-height:var(--hit-target-compact);padding:0 6px;color:var(--color-on-toast);white-space:nowrap}
  :global(.toast-frame .ui-button.quiet:hover){background:var(--color-on-toast-hover)}
  svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round;stroke-linejoin:round}
</style>
