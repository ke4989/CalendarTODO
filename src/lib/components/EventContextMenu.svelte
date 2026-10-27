<script lang="ts">
  import { onMount, onDestroy } from "svelte";

  let { x, y, label, choose, close, opener }: {
    x: number;
    y: number;
    label: string;
    choose: () => void;
    close: () => void;
    opener: HTMLElement | null;
  } = $props();
  let menu: HTMLDivElement;
  let left = $state(0);
  let top = $state(0);

  function outside(event: PointerEvent) { if (!menu.contains(event.target as Node)) close(); }
  function keydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault(); event.stopPropagation();
      const target = opener;
      close();
      queueMicrotask(() => {
        if (target?.isConnected && !target.matches(":disabled") && !target.closest("[inert]")) target.focus({ preventScroll: true });
      });
    }
  }
  onMount(() => {
    const rect = menu.getBoundingClientRect();
    left = Math.max(8, Math.min(x, window.innerWidth - rect.width - 8));
    top = Math.max(8, Math.min(y, window.innerHeight - rect.height - 8));
    menu.querySelector<HTMLButtonElement>("button")?.focus();
    document.addEventListener("pointerdown", outside);
  });
  onDestroy(() => document.removeEventListener("pointerdown", outside));
</script>

<div bind:this={menu} class="event-context-menu" role="menu" tabindex="-1" style:left={`${left}px`} style:top={`${top}px`} onkeydown={keydown}>
  <button type="button" role="menuitem" onclick={choose}>{label}</button>
</div>

<style>
  .event-context-menu{position:fixed;z-index:40;min-width:136px;padding:5px;border:0;border-radius:var(--radius-row);background:var(--color-surface);box-shadow:var(--shadow-menu);outline:0}
  button{display:block;width:100%;min-height:var(--hit-target-compact);padding:7px 10px;border:0;border-radius:7px;text-align:left;color:var(--color-danger);background:transparent;font:inherit;cursor:pointer}
  button:hover,button:focus-visible{background:var(--color-danger-surface)}
  button:focus-visible{outline:0;box-shadow:none}
</style>
