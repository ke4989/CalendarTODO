<script lang="ts">
  import type { Snippet } from "svelte";
  let { children }: { children: Snippet } = $props();
</script>
<div class="toast-viewport">{@render children()}</div>
<style>
  .toast-viewport{position:fixed;z-index:100;top:var(--space-3);left:50%;transform:translateX(-50%);display:grid;gap:var(--space-2);width:max-content;max-width:calc(100vw - 24px);max-height:calc(100vh - 24px);overflow-y:auto;pointer-events:none;padding:3px}
  .toast-viewport :global(.toast-frame){pointer-events:auto}
</style>
