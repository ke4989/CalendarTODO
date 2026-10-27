<script lang="ts">
  import type { Snippet } from "svelte";

  let { children, action, variant = "info", live }: {
    children: Snippet;
    action?: Snippet;
    variant?: "info" | "success" | "error";
    live?: "off" | "polite" | "assertive";
  } = $props();

  const role = $derived(variant === "error" ? "alert" : "status");
  const ariaLive = $derived(live ?? (variant === "error" ? "assertive" : "polite"));
</script>

<div class={`inline-status ${variant}`} role={role} aria-live={ariaLive}>
  <div class="copy">{@render children()}</div>
  {#if action}<div class="action">{@render action()}</div>{/if}
</div>

<style>
  .inline-status{display:flex;align-items:center;justify-content:space-between;gap:var(--space-3);margin:0;padding:var(--space-3);border-radius:var(--radius-row);font-size:var(--font-size-meta);line-height:var(--line-height-body)}
  .copy{min-width:0}
  .action{flex:0 0 auto}
  .info{color:var(--color-info-text);background:var(--color-info-surface)}
  .success{color:var(--color-success-text);background:var(--color-success-surface)}
  .error{color:var(--color-danger-text);background:var(--color-danger-surface)}
</style>
