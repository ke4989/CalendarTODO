<script lang="ts">
  import type { Snippet } from "svelte";

  let { children, action, variant = "info", duration = "standard", stacked = false }: {
    stacked?: boolean;
    children: Snippet;
    action?: Snippet;
    variant?: "info" | "success" | "error";
    duration?: "standard" | "undo";
  } = $props();

  const role = $derived(variant === "error" ? "alert" : "status");
</script>

<div class={`toast-frame ${variant} ${duration}`} class:stacked role={role} aria-live={variant === "error" ? "assertive" : "polite"}>
  <span class="message">{@render children()}</span>
  {#if action}<span class="action">{@render action()}</span>{/if}
</div>

<style>
  .toast-frame{position:fixed;z-index:21;top:var(--space-3);left:50%;transform:translateX(-50%);display:flex;align-items:center;gap:var(--space-3);max-width:calc(100vw - 24px);padding:10px 14px;border:1px solid color-mix(in srgb,currentColor 36%,transparent);border-radius:var(--radius-row);color:var(--color-on-toast);animation:toast-standard 3s both var(--ease-out);white-space:nowrap}
  .toast-frame.stacked{position:static;transform:none;animation:none;opacity:1}
  .message{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis}
  .action{flex:0 0 auto;margin-left:auto}
  .info{background:var(--color-toast-info)}.success{background:var(--color-toast-success)}.error{background:var(--color-toast-error)}
  .undo{animation-name:toast-undo;animation-duration:5s}
  @keyframes toast-standard{0%{opacity:0;transform:translate(-50%,-8px)}7%,86%{opacity:1;transform:translate(-50%,0)}100%{opacity:0;transform:translate(-50%,-6px)}}
  @keyframes toast-undo{0%{opacity:0;transform:translate(-50%,-8px)}5%,90%{opacity:1;transform:translate(-50%,0)}100%{opacity:0;transform:translate(-50%,-6px)}}
  @media(prefers-reduced-motion:reduce){.toast-frame{animation-name:toast-opacity;transform:translateX(-50%)}@keyframes toast-opacity{0%,100%{opacity:0}7%,86%{opacity:1}}.toast-frame.undo{animation-name:toast-undo-opacity}@keyframes toast-undo-opacity{0%,100%{opacity:0}5%,90%{opacity:1}}}
</style>
