<script lang="ts">
  import type { Snippet } from "svelte";
  import { createBackdropClickGuard, dialogFocus } from "$lib/ui/dialog-focus";
  const backdrop = createBackdropClickGuard();

  let { id, title, closeLabel, close, children, footer, disabled = false, size = "regular", ariaLabel }: {
    id: string;
    title: string;
    closeLabel: string;
    close: () => void;
    children: Snippet;
    footer?: Snippet;
    disabled?: boolean;
    size?: "compact" | "regular";
    ariaLabel?: string;
  } = $props();
</script>

<div class="dialog-scrim" role="presentation" onpointerdown={(event) => backdrop.pointerDown(event.target,event.currentTarget)} onpointerup={(event) => backdrop.pointerUp(event.target,event.currentTarget)} onpointercancel={backdrop.pointerCancel} onclick={(event) => { if (backdrop.click(event.target,event.currentTarget) && !disabled) close(); }}>
  <div class={`dialog-shell ${size}`} role="dialog" aria-modal="true" aria-labelledby={ariaLabel ? undefined : id} aria-label={ariaLabel} tabindex="-1" use:dialogFocus={{ close, disabled }}>
    <header><h2 {id}>{title}</h2><button type="button" aria-label={closeLabel} title={closeLabel} {disabled} onclick={close}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18" /></svg></button></header>
    <div class="body">{@render children()}</div>
    {#if footer}<footer>{@render footer()}</footer>{/if}
  </div>
</div>

<style>
  .dialog-scrim{position:fixed;inset:0;z-index:8;display:grid;place-items:center;padding:18px;background:var(--color-scrim)}
  .dialog-shell{width:min(100%,520px);max-height:calc(100vh - 36px);display:flex;flex-direction:column;overflow:hidden;border:1px solid var(--color-border);border-radius:var(--radius-dialog);color:var(--color-text);background:var(--color-surface);box-shadow:var(--shadow-dialog)}
  .dialog-shell.compact{width:min(100%,390px)}
  header{flex:0 0 auto;display:flex;align-items:center;justify-content:space-between;gap:var(--space-3);padding:var(--space-3) var(--space-4);border-bottom:1px solid var(--color-border)}
  h2{margin:0;font-size:1rem;line-height:var(--line-height-tight)}
  header button{width:var(--hit-target-compact);height:var(--hit-target-compact);display:grid;place-items:center;padding:0;border:0;border-radius:var(--radius-control);color:var(--color-text-muted);background:transparent}
  header button:hover:not(:disabled){color:var(--color-text);background:var(--color-surface-soft)}
  header svg{width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round}
  .body{flex:1 1 auto;min-height:0;padding:var(--space-4);overflow-y:auto;overscroll-behavior:contain}
  footer{flex:0 0 auto;display:flex;justify-content:flex-end;gap:var(--space-2);padding:10px var(--space-4);border-top:1px solid var(--color-border);background:var(--color-surface-soft)}
</style>
