<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import IconButton from "$lib/components/IconButton.svelte";
  import { addTag, matchingTags } from "$lib/ui/tag-input";
  import { translate, type ResolvedLanguage } from "$lib/i18n";

  let { id, tags, suggestions, onTags, disabled = false, language = "zh-CN" }: {
    id: string;
    tags: string[];
    suggestions: string[];
    onTags: (tags: string[]) => void;
    disabled?: boolean;
    language?: ResolvedLanguage;
  } = $props();
  let value = $state("");
  let open = $state(false);
  let activeIndex = $state(-1);
  let root: HTMLDivElement;
  let input: HTMLInputElement;
  let form: HTMLFormElement | null = null;
  const matches = $derived(matchingTags(suggestions, tags, value));

  $effect(() => {
    if (!open || matches.length === 0) activeIndex = -1;
    else if (activeIndex >= matches.length) activeIndex = matches.length - 1;
  });

  $effect(() => { if (disabled) { open = false; activeIndex = -1; } });

  function commit(candidate = value) {
    if (disabled) return;
    const next = addTag(tags, candidate);
    if (next !== tags) onTags(next);
    value = "";
    open = false;
    activeIndex = -1;
  }
  function remove(tag: string) { onTags(tags.filter((item) => item !== tag)); }
  function onInput(event: Event) { value = (event.currentTarget as HTMLInputElement).value; open = !!value.trim(); activeIndex = -1; }
  function onKeydown(event: KeyboardEvent) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      open = true;
      if (matches.length) activeIndex = event.key === "ArrowDown" ? (activeIndex + 1) % matches.length : (activeIndex <= 0 ? matches.length - 1 : activeIndex - 1);
    } else if (event.key === "Home" && open && matches.length) {
      event.preventDefault(); activeIndex = 0;
    } else if (event.key === "End" && open && matches.length) {
      event.preventDefault(); activeIndex = matches.length - 1;
    } else if (event.key === "Enter") {
      event.preventDefault(); commit(activeIndex >= 0 ? matches[activeIndex] : value);
    } else if (event.key === "Escape" && open) {
      event.preventDefault(); event.stopPropagation(); open = false; activeIndex = -1;
    }
  }
  function toggleOptions() { open = !open; activeIndex = -1; input.focus(); }
  function commitDraft() { if (!disabled) commit(); }
  function onFocusout(event: FocusEvent) {
    if (event.relatedTarget instanceof Node && root.contains(event.relatedTarget)) return;
    commitDraft();
  }
  function closeFromOutside(event: PointerEvent) { if (!root.contains(event.target as Node)) { open = false; activeIndex = -1; } }
  onMount(() => {
    form = root.closest("form");
    form?.addEventListener("submit", commitDraft, true);
    document.addEventListener("pointerdown", closeFromOutside);
  });
  onDestroy(() => {
    form?.removeEventListener("submit", commitDraft, true);
    document.removeEventListener("pointerdown", closeFromOutside);
  });
</script>

<div class="tag-input" bind:this={root} onfocusout={onFocusout}>
  <div class="controls">
    <div class="tag-entry">
      {#if tags.length}<div class="chips" aria-label={translate(language, "selectedTags")}>{#each tags as tag (tag)}<span>{tag}<IconButton size="compact" label={`${translate(language, "removeTag")} ${tag}`} disabled={disabled} onclick={() => remove(tag)}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 7l10 10M17 7 7 17" /></svg></IconButton></span>{/each}</div>{/if}
      <input bind:this={input} {id} type="text" role="combobox" aria-label={translate(language, "tagName")} aria-autocomplete="list" aria-expanded={open} aria-controls={`${id}-options`} aria-activedescendant={open && activeIndex >= 0 ? `${id}-option-${activeIndex}` : undefined} autocomplete="off" maxlength="64" {disabled} value={value} oninput={onInput} onkeydown={onKeydown} />
    </div>
    <IconButton class="dropdown" variant="outlined" label={translate(language, "showTags")} aria-expanded={open} disabled={disabled} onclick={toggleOptions}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 10 5 5 5-5" /></svg></IconButton>
    <IconButton class="add" variant="outlined" label={translate(language, "addTag")} disabled={disabled || !value.trim() || tags.length >= 32} onclick={() => commit()}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14" /></svg></IconButton>
    {#if open}
      <div class="options" id={`${id}-options`} role="listbox" aria-label={translate(language, "existingTags")}>
        {#each matches as suggestion, index (suggestion)}<button id={`${id}-option-${index}`} type="button" role="option" aria-selected={activeIndex === index} tabindex="-1" class:active={activeIndex === index} {disabled} onpointerdown={(event) => event.preventDefault()} onclick={() => commit(suggestion)}>{suggestion}</button>{/each}
        {#if matches.length === 0}<span>{translate(language, "noTags")}</span>{/if}
      </div>
    {/if}
    <span class="sr-only" aria-live="polite">{open ? `${matches.length} ${translate(language, "existingTags")}` : ""}</span>
  </div>
</div>

<style>
  .tag-input{min-width:0}.controls{position:relative;display:grid;grid-template-columns:minmax(0,1fr) var(--hit-target-compact) var(--hit-target-compact);align-items:center;gap:2px;min-width:0;padding:2px;border-radius:var(--radius-control);background:color-mix(in srgb,var(--color-primary) 5%,var(--color-surface-soft))}.tag-entry{min-width:0;display:flex;align-items:center;flex-wrap:wrap;gap:4px}.chips{display:contents}.chips span{display:inline-flex;align-items:center;gap:2px;max-width:100%;padding:2px 3px 2px 9px;border:0;border-radius:var(--radius-pill);color:var(--color-text);background:color-mix(in srgb,var(--color-primary) 9%,var(--color-surface));font-size:var(--font-size-caption);overflow-wrap:anywhere}.controls input,.controls input:focus,.controls input:focus-visible{min-width:100px;flex:1 1 120px;border:0!important;outline:0!important;background:transparent;box-shadow:none!important}.controls:focus-within{background:color-mix(in srgb,var(--color-primary) 10%,var(--color-surface));box-shadow:none}.controls :global(.icon-button.outlined){border:0;background:transparent}.options{position:absolute;z-index:20;top:calc(100% + 5px);left:0;right:0;max-height:180px;overflow-y:auto;padding:5px;border:1px solid var(--color-border);border-radius:var(--radius-row);background:var(--color-surface);box-shadow:var(--shadow-menu)}.options button,.options span{display:block;width:100%;padding:8px;border:0;border-radius:6px;text-align:left;color:var(--color-text);background:transparent}.options button:hover:not(:disabled),.options button.active{background:var(--color-surface-soft)}.options button:focus-visible{outline:2px solid var(--color-focus);outline-offset:-2px}.options span{color:var(--color-text-muted)}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}
</style>
