<script lang="ts">
  import { onMount, tick } from "svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";

  type Command = "undo" | "redo" | "cut" | "copy" | "paste" | "selectAll";
  type TextControl = HTMLInputElement | HTMLTextAreaElement;
  type EditContext = { target: TextControl; start: number | null; end: number | null; enabled: Set<Command> };
  const actions: { command: Command; shortcut: string; separated?: boolean }[] = [
    { command: "undo", shortcut: "Ctrl+Z" },
    { command: "redo", shortcut: "Ctrl+Y" },
    { command: "cut", shortcut: "Ctrl+X", separated: true },
    { command: "copy", shortcut: "Ctrl+C" },
    { command: "paste", shortcut: "Ctrl+V" },
    { command: "selectAll", shortcut: "Ctrl+A", separated: true },
  ];
  let context = $state.raw<EditContext | null>(null);
  let menu = $state<HTMLDivElement>();
  let left = $state(0);
  let top = $state(0);
  let language = $state<ResolvedLanguage>("zh-CN");
  let error = $state("");

  function restoreSelection(saved: EditContext) {
    saved.target.focus({ preventScroll: true });
    if (saved.start !== null && saved.end !== null) saved.target.setSelectionRange(saved.start, saved.end);
  }
  function close(restore = false) {
    const saved = context;
    context = null;
    if (restore && saved?.target.isConnected) restoreSelection(saved);
  }
  async function open(event: MouseEvent) {
    const target = event.target;
    if (!(target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) || target.disabled) return;
    if (target instanceof HTMLInputElement && /^(button|submit|reset|checkbox|radio|range|color|file|hidden|image)$/.test(target.type)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    target.focus({ preventScroll: true });
    const writable = !target.readOnly;
    const secret = target instanceof HTMLInputElement && target.type === "password";
    const enabled = new Set<Command>();
    for (const command of ["undo", "redo", "cut", "copy"] as const) {
      if ((command === "copy" || writable) && (!secret || command === "undo" || command === "redo") && document.queryCommandEnabled(command)) enabled.add(command);
    }
    if (writable) enabled.add("paste");
    if (target.value) enabled.add("selectAll");
    language = document.documentElement.lang.startsWith("zh") ? "zh-CN" : "en";
    error = "";
    context = { target, start: target.selectionStart, end: target.selectionEnd, enabled };
    const rect = target.getBoundingClientRect();
    left = event.clientX || rect.left;
    top = event.clientY || rect.bottom;
    await tick();
    if (!context || !menu) return;
    left = Math.max(8, Math.min(left, window.innerWidth - menu.offsetWidth - 8));
    top = Math.max(8, Math.min(top, window.innerHeight - menu.offsetHeight - 8));
    menu.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus({ preventScroll: true });
  }
  async function choose(command: Command) {
    const saved = context;
    if (!saved || !saved.enabled.has(command) || !saved.target.isConnected) return;
    restoreSelection(saved);
    if (command === "selectAll") {
      saved.target.select();
    } else if (command === "paste") {
      try {
        const text = await navigator.clipboard.readText();
        if (context !== saved || !saved.target.isConnected) return;
        restoreSelection(saved);
        if (text && !document.execCommand("insertText", false, text)) throw new Error("insertText unavailable");
      } catch {
        error = translate(language, "clipboardFailed");
        return;
      }
    } else if (!document.execCommand(command)) {
      error = translate(language, "clipboardFailed");
      return;
    }
    close();
  }
  function keydown(event: KeyboardEvent) {
    if (!context || !menu) return;
    if (event.ctrlKey && !event.altKey) {
      const command = ({ z: event.shiftKey ? "redo" : "undo", y: "redo", x: "cut", c: "copy", v: "paste", a: "selectAll" } as const)[event.key.toLowerCase() as "z" | "y" | "x" | "c" | "v" | "a"];
      if (command) {
        event.preventDefault(); event.stopImmediatePropagation(); void choose(command); return;
      }
    }
    if (event.key === "Escape" || event.key === "Tab") {
      event.preventDefault(); event.stopImmediatePropagation(); close(true); return;
    }
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    event.preventDefault(); event.stopImmediatePropagation();
    const buttons = [...menu.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === "Home" ? 0 : event.key === "End" ? buttons.length - 1 : (index + (event.key === "ArrowDown" ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus();
  }
  onMount(() => {
    const outside = (event: PointerEvent) => { if (context && menu && !menu.contains(event.target as Node)) close(); };
    const blur = () => close();
    document.addEventListener("contextmenu", open, true);
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", keydown, true);
    window.addEventListener("blur", blur);
    return () => {
      document.removeEventListener("contextmenu", open, true);
      document.removeEventListener("pointerdown", outside, true);
      document.removeEventListener("keydown", keydown, true);
      window.removeEventListener("blur", blur);
    };
  });
</script>

{#if context}
  <div bind:this={menu} class="text-edit-menu" role="menu" aria-label={translate(language, "textEditMenu")} tabindex="-1" style:left={`${left}px`} style:top={`${top}px`}>
    {#each actions as action}
      <button class:separated={action.separated} type="button" role="menuitem" disabled={!context.enabled.has(action.command)} onpointerdown={(event) => { event.preventDefault(); event.stopPropagation(); }} onclick={() => void choose(action.command)}>
        <span>{translate(language, action.command)}</span><kbd>{action.shortcut}</kbd>
      </button>
    {/each}
    {#if error}<p role="alert">{error}</p>{/if}
  </div>
{/if}

<style>
  .text-edit-menu{position:fixed;z-index:80;width:224px;max-width:calc(100vw - 16px);padding:5px;border:0;border-radius:var(--radius-row);color:var(--color-text);background:var(--color-surface);box-shadow:var(--shadow-menu);outline:0}
  button{display:flex;align-items:center;justify-content:space-between;gap:24px;width:100%;min-height:var(--hit-target-compact);padding:7px 10px;border:0;border-radius:7px;color:inherit;background:transparent;font:inherit;font-size:var(--font-size-meta);text-align:left;cursor:pointer}
  button:hover:not(:disabled),button:focus-visible{background:var(--color-surface-soft);outline:0;box-shadow:none}
  button:disabled{color:var(--color-text-muted);opacity:.5;cursor:default}
  button.separated{margin-top:5px}
  kbd{color:var(--color-text-muted);font:inherit;font-size:var(--font-size-caption)}
  p{margin:6px 10px;font-size:var(--font-size-caption);line-height:1.4}
</style>
