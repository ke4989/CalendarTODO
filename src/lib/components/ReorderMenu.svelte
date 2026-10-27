<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  import type { ReorderAction } from "$lib/ui/reorder-menu";

  let { x, y, disabled, choose, close, language = "zh-CN" }: {
    x: number;
    y: number;
    disabled: Record<ReorderAction, boolean>;
    choose: (action: ReorderAction) => void;
    close: () => void;
    language?: ResolvedLanguage;
  } = $props();
  let menu: HTMLDivElement;
  let left = $state(0);
  let top = $state(0);
  const actions: { action: ReorderAction; key: "moveUp" | "moveDown" | "moveToTop" | "moveToBottom" }[] = [
    { action: "up", key: "moveUp" },
    { action: "down", key: "moveDown" },
    { action: "top", key: "moveToTop" },
    { action: "bottom", key: "moveToBottom" }
  ];

  function outside(event: PointerEvent) { if (!menu.contains(event.target as Node)) close(); }
  function keydown(event: KeyboardEvent) {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(); }
    if (!["ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const items = [...menu.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
    if (!items.length) return;
    const current = Math.max(0, items.indexOf(document.activeElement as HTMLButtonElement));
    const next = event.key === "Home" ? 0 : event.key === "End" ? items.length - 1
      : event.key === "ArrowUp" ? (current - 1 + items.length) % items.length
        : (current + 1) % items.length;
    items[next].focus();
  }
  onMount(() => {
    const rect = menu.getBoundingClientRect();
    left = Math.max(8, Math.min(x, window.innerWidth - rect.width - 8));
    top = Math.max(8, Math.min(y, window.innerHeight - rect.height - 8));
    menu.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
    document.addEventListener("pointerdown", outside);
  });
  onDestroy(() => document.removeEventListener("pointerdown", outside));
</script>

<div bind:this={menu} class="reorder-menu" role="menu" tabindex="-1" aria-label={translate(language, "reorderActions")} style:left={`${left}px`} style:top={`${top}px`} onkeydown={keydown}>
  {#each actions as item}
    <button type="button" role="menuitem" disabled={disabled[item.action]} onclick={() => choose(item.action)}>{translate(language, item.key)}</button>
  {/each}
</div>

<style>
  .reorder-menu{position:fixed;z-index:40;min-width:148px;padding:5px;border:0;border-radius:var(--radius-row);background:var(--color-surface);box-shadow:var(--shadow-menu);outline:0}
  button{display:block;width:100%;min-height:var(--hit-target-compact);padding:7px 10px;border:0;border-radius:7px;text-align:left;color:var(--color-text);background:transparent;font:inherit;cursor:pointer}
  button:hover:not(:disabled),button:focus-visible{background:var(--color-surface-soft)}
  button:focus-visible{outline:1px solid var(--color-focus);outline-offset:-1px;box-shadow:none}
  button:disabled{opacity:.42;cursor:default}
</style>
