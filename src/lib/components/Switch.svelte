<script lang="ts">
  let { id, label, labelVisible = true, checked = $bindable(false), disabled = false, onChange }: {
    id?: string;
    label: string;
    labelVisible?: boolean;
    checked?: boolean;
    disabled?: boolean;
    onChange?: (checked: boolean) => void;
  } = $props();
</script>

<label class="switch" class:disabled>
  <input {id} type="checkbox" role="switch" bind:checked {disabled} onchange={(event) => onChange?.(event.currentTarget.checked)} />
  <span class="track" aria-hidden="true"><span class="thumb"></span></span>
  <span class:sr-only={!labelVisible} class="label">{label}</span>
</label>

<style>
  .switch{min-height:var(--hit-target);display:inline-flex;align-items:center;gap:var(--space-3);margin:0;color:var(--color-text);font-size:var(--font-size-meta);font-weight:600;cursor:pointer}
  .switch.disabled{cursor:not-allowed;opacity:.55}
  input{position:absolute;width:1px;height:1px;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap}
  .track{position:relative;width:38px;height:22px;flex:0 0 38px;border:1px solid var(--color-border-strong);border-radius:var(--radius-pill);background:var(--color-surface-soft);transition:background-color var(--duration-fast) var(--ease-standard),border-color var(--duration-fast) var(--ease-standard)}
  .thumb{position:absolute;top:3px;left:3px;width:14px;height:14px;border-radius:50%;background:var(--color-text-muted);transition:transform var(--duration-fast) var(--ease-out),background-color var(--duration-fast) var(--ease-standard)}
  input:checked + .track{border-color:var(--color-primary);background:var(--color-primary)}
  input:checked + .track .thumb{transform:translateX(16px);background:var(--color-on-primary)}
  input:focus-visible + .track{outline:2px solid var(--color-focus);outline-offset:2px;box-shadow:var(--focus-ring)}
  .label{line-height:var(--line-height-body)}
  .sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}
</style>
