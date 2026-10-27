<script lang="ts">
  import type { Snippet } from "svelte";

  let { title, description, actions, level = 2, id }: {
    title: string;
    description?: string;
    actions?: Snippet;
    level?: 2 | 3 | 4;
    id?: string;
  } = $props();
</script>

<header class="section-header">
  <div class="copy">
    {#if level === 2}<h2 {id}>{title}</h2>{:else if level === 3}<h3 {id}>{title}</h3>{:else}<h4 {id}>{title}</h4>{/if}
    {#if description}<p>{description}</p>{/if}
  </div>
  {#if actions}<div class="actions">{@render actions()}</div>{/if}
</header>

<style>
  .section-header{display:flex;align-items:flex-start;justify-content:space-between;gap:var(--space-4);margin:0 0 var(--space-3)}
  .copy{min-width:0}
  h2,h3,h4{margin:0;color:var(--color-text);line-height:var(--line-height-tight)}
  h2{font-size:var(--font-size-section)}h3{font-size:1rem}h4{font-size:var(--font-size-body)}
  p{max-width:70ch;margin:var(--space-1) 0 0;color:var(--color-text-muted);font-size:var(--font-size-meta);line-height:var(--line-height-body)}
  .actions{flex:0 0 auto}
</style>
