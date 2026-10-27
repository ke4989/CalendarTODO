<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLButtonAttributes } from "svelte/elements";

  type Props = Omit<HTMLButtonAttributes, "children" | "class" | "title"> & {
    children: Snippet;
    label: string;
    title?: string;
    variant?: "quiet" | "outlined" | "danger";
    size?: "compact" | "regular";
    class?: string;
  };

  let { children, label, title, variant = "quiet", size = "regular", class: className = "", ...attributes }: Props = $props();
</script>

<button {...attributes} class={`icon-button ${variant} ${size} ${className}`.trim()} aria-label={label} title={title ?? label}>
  {@render children()}
</button>

<style>
  .icon-button{width:var(--hit-target);height:var(--hit-target);min-width:var(--hit-target);min-height:var(--hit-target);display:grid;place-items:center;padding:0;border:1px solid transparent;border-radius:var(--radius-control);color:var(--color-text-muted);background:transparent;transition:background-color var(--duration-fast) var(--ease-standard),border-color var(--duration-fast) var(--ease-standard),color var(--duration-fast) var(--ease-standard),transform var(--duration-instant) var(--ease-standard)}
  .icon-button:hover:not(:disabled){color:var(--color-text);background:var(--color-surface-soft)}
  .icon-button:active:not(:disabled){transform:translateY(1px)}
  .compact{width:var(--hit-target-compact);height:var(--hit-target-compact);min-width:var(--hit-target-compact);min-height:var(--hit-target-compact)}
  .outlined{border-color:var(--color-border);color:var(--color-text);background:var(--color-surface)}
  .outlined:hover:not(:disabled){border-color:var(--color-border-strong)}
  .danger{color:var(--color-danger)}
  :global(.icon-button > svg){width:18px;height:18px;display:block;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round;stroke-linejoin:round}
</style>
