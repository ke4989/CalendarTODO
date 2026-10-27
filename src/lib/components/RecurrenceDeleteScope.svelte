<script lang="ts">
  import DialogShell from "$lib/components/DialogShell.svelte";
  import Button from "$lib/components/Button.svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  let { language, title, close, choose }: { language: ResolvedLanguage; title: string; close: () => void; choose: (scope: "occurrence" | "series") => void } = $props();
</script>

<DialogShell id="recurrence-delete-title" title={translate(language,"deleteRecurringTitle")} closeLabel={translate(language,"close")} {close} size="compact">
  <p>{title}</p>
  <div class="choices">
    <Button type="button" variant="secondary" onclick={() => choose("occurrence")}>{translate(language,"deleteThisOccurrence")}</Button>
    <Button type="button" variant="danger" onclick={() => choose("series")}>{translate(language,"deleteEntireSeries")}</Button>
  </div>
</DialogShell>

<style>
  p{margin:0 0 14px;color:var(--color-text-muted);overflow-wrap:anywhere}.choices{display:grid;gap:8px}.choices :global(.ui-button){width:100%}
</style>
