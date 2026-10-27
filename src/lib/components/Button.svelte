<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLButtonAttributes } from "svelte/elements";

  type Props = Omit<HTMLButtonAttributes, "children" | "class"> & {
    children: Snippet;
    variant?: "primary" | "secondary" | "quiet" | "danger";
    size?: "compact" | "regular";
    class?: string;
  };

  let { children, variant = "secondary", size = "regular", class: className = "", ...attributes }: Props = $props();
</script>

<button {...attributes} class={`ui-button ${variant} ${size} ${className}`.trim()}>
  {@render children()}
</button>

<style>
  .ui-button{min-width:0;min-height:var(--hit-target);display:inline-flex;align-items:center;justify-content:center;gap:var(--space-2);padding:0 var(--space-4);border:1px solid transparent;border-radius:var(--radius-control);font:inherit;font-weight:600;line-height:1.25;text-align:center;white-space:normal;overflow-wrap:anywhere;transition:background-color var(--duration-fast) var(--ease-standard),border-color var(--duration-fast) var(--ease-standard),color var(--duration-fast) var(--ease-standard),transform var(--duration-instant) var(--ease-standard)}
  .ui-button:active:not(:disabled){transform:translateY(1px)}
  .compact{min-height:var(--hit-target-compact);padding:0 var(--space-3);font-size:var(--font-size-meta)}
  .primary{border-color:var(--color-primary);color:var(--color-on-primary);background:var(--color-primary)}
  .primary:hover:not(:disabled){border-color:var(--color-primary-hover);background:var(--color-primary-hover)}
  .primary:active:not(:disabled){border-color:var(--color-primary-pressed);background:var(--color-primary-pressed)}
  .secondary{border-color:var(--color-border);color:var(--color-text);background:var(--color-surface)}
  .secondary:hover:not(:disabled){border-color:var(--color-border-strong);background:var(--color-surface-soft)}
  .quiet{color:var(--color-text-muted);background:transparent}
  .quiet:hover:not(:disabled){color:var(--color-text);background:var(--color-surface-soft)}
  .danger{border-color:var(--color-danger);color:var(--color-on-danger);background:var(--color-danger)}
  .danger:hover:not(:disabled){border-color:var(--color-danger-hover);background:var(--color-danger-hover)}
</style>
