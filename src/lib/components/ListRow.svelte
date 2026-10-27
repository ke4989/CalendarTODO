<script lang="ts">
  import type { Snippet } from "svelte";

  let { children, leading, trailing, selected = false, muted = false }: {
    children: Snippet;
    leading?: Snippet;
    trailing?: Snippet;
    selected?: boolean;
    muted?: boolean;
  } = $props();
</script>

<div class="list-row" class:selected class:muted>
  {#if leading}<div class="leading">{@render leading()}</div>{/if}
  <div class="content">{@render children()}</div>
  {#if trailing}<div class="trailing">{@render trailing()}</div>{/if}
</div>

<style>
  .list-row{min-width:0;display:flex;align-items:center;gap:var(--space-3);padding:var(--space-3);border-radius:var(--radius-row);color:var(--color-text);background:transparent;transition:background-color var(--duration-fast) var(--ease-standard)}
  .list-row:hover{background:var(--color-surface-soft)}.list-row.selected{background:color-mix(in srgb,var(--color-primary) 12%,var(--color-surface))}.list-row.muted{color:var(--color-text-muted)}
  .leading,.trailing{flex:0 0 auto}.content{min-width:0;flex:1}.trailing{display:flex;align-items:center;gap:var(--space-1)}
</style>
