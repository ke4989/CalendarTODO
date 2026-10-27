<script lang="ts">
  import type { Snippet } from "svelte";

  let { label, description, control, children, wide = false }: {
    label: string;
    description?: string;
    control?: Snippet;
    children?: Snippet;
    wide?: boolean;
  } = $props();
</script>

<div class="settings-row" class:wide>
  <div class="copy"><strong>{label}</strong>{#if description}<span>{description}</span>{/if}</div>
  {#if control}<div class="control">{@render control()}</div>{/if}
  {#if children}<div class="content">{@render children()}</div>{/if}
</div>

<style>
  .settings-row{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:center;gap:var(--space-3) var(--space-4);min-height:var(--hit-target);padding:var(--space-3) 0;border-bottom:1px solid var(--color-border)}
  .settings-row:last-child{border-bottom:0}
  .settings-row.wide{grid-column:1/-1}
  .copy{min-width:0;display:grid;gap:2px}.copy strong{font-size:var(--font-size-body);font-weight:600;line-height:var(--line-height-tight)}.copy span{color:var(--color-text-muted);font-size:var(--font-size-meta);line-height:var(--line-height-body)}
  .control{justify-self:end}.content{grid-column:1/-1;min-width:0}
</style>
