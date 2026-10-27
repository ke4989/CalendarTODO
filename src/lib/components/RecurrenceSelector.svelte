<script lang="ts">
  import Button from "$lib/components/Button.svelte";
  import { onMount, onDestroy, tick } from "svelte";
  import type { RecurrenceFrequency, RecurrenceOptions } from "$lib/ipc/contracts";
  import type { ResolvedLanguage } from "$lib/i18n";
  import { copyRecurrenceOptions, defaultRecurrenceOptions, recurrenceSummary } from "$lib/ui/recurrence";

  type Frequency = "none" | RecurrenceFrequency;
  let { language, frequency, interval, options, anchorAt, disabled = false, change }: {
    language: ResolvedLanguage; frequency: Frequency; interval: number; options: RecurrenceOptions;
    anchorAt: number | null; disabled?: boolean;
    change: (frequency: Frequency, interval: number, options: RecurrenceOptions) => void;
  } = $props();

  const zh = $derived(language === "zh-CN");
  const weekdayLabels = $derived(zh ? ["一", "二", "三", "四", "五", "六", "日"] : ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]);
  let open = $state(false);
  let anchor: HTMLDivElement;
  let panel = $state<HTMLDivElement>();
  let left = $state(8), top = $state(8);
  let observer: ResizeObserver | undefined;
  let side = $state<"above" | "below">();
  let maxHeight = $state(240);
  $effect(() => { if (disabled && open) { if (panel) observer?.unobserve(panel); open = false; } });

  function position() {
    if (!open || !panel || !anchor) return;
    const rect = anchor.getBoundingClientRect();
    const width = document.documentElement.clientWidth;
    const height = document.documentElement.clientHeight;
    const below = Math.max(0, height - rect.bottom - 14), above = Math.max(0, rect.top - 14);
    side ??= below >= 240 || below >= above ? "below" : "above";
    maxHeight = Math.max(0, Math.min(height - 16, side === "below" ? below : above));
    panel.style.maxHeight = `${maxHeight}px`;
    const bounds = panel.getBoundingClientRect();
    left = Math.max(8, Math.min(rect.left, width - bounds.width - 8));
    top = side === "below" ? Math.max(8, rect.bottom + 6) : Math.max(8, rect.top - bounds.height - 6);
  }
  function resize() { side = undefined; position(); }
  function close() { if (panel) observer?.unobserve(panel); open = false; anchor?.querySelector<HTMLButtonElement>("button")?.focus(); }
  function outside(event: PointerEvent) {
    if (open && !panel?.contains(event.target as Node) && !anchor?.contains(event.target as Node)) { if (panel) observer?.unobserve(panel); open = false; }
  }
  function key(event: KeyboardEvent) {
    if (open && event.key === "Escape") { event.preventDefault(); event.stopImmediatePropagation(); close(); }
    else if (open && event.key === "Enter" && event.target instanceof HTMLInputElement && panel?.contains(event.target)) { event.preventDefault(); event.stopImmediatePropagation(); apply(); }
  }
  onMount(() => {
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", key, true);
    document.addEventListener("scroll", position, true);
    window.addEventListener("resize", resize);
    observer = new ResizeObserver(position);
    return () => observer?.disconnect();
  });
  onDestroy(() => {
    document.removeEventListener("pointerdown", outside);
    document.removeEventListener("keydown", key, true);
    document.removeEventListener("scroll", position, true);
    window.removeEventListener("resize", resize);
  });
  let draftFrequency = $state<Frequency>("none");
  let draftInterval = $state(1);
  let draftOptions = $state<RecurrenceOptions>(defaultRecurrenceOptions());
  let monthDaysText = $state("");

  function defaults(next: Frequency): RecurrenceOptions {
    const date = new Date(anchorAt ?? Date.now());
    return {
      ...defaultRecurrenceOptions(),
      weekdays: next === "customWeekly" ? [(date.getDay() + 6) % 7] : [],
      monthDays: next === "customMonthly" ? [date.getDate()] : [],
      yearMonths: next === "customYearly" ? [date.getMonth() + 1] : [],
      endKind: "count", endCount: 100, endUntilAtUtcMs: null,
    };
  }

  async function show() {
    if (disabled) return;
    side = undefined;
    if (open) { close(); return; }
    draftFrequency = frequency;
    draftInterval = interval;
    draftOptions = copyRecurrenceOptions(options);
    monthDaysText = draftOptions.monthDays.join(", ");
    open = true;
    await tick();
    if (!panel) return;
    panel.showPopover();
    position();
    observer?.observe(panel);
    panel.querySelector<HTMLSelectElement>("select")?.focus();
  }
  function setFrequency(value: Frequency) {
    draftFrequency = value;
    if (value !== "none") {
      draftOptions = defaults(value);
      monthDaysText = draftOptions.monthDays.join(", ");
    }
  }
  function patch(value: Partial<RecurrenceOptions>) { draftOptions = { ...draftOptions, ...value }; }
  function toggle(list: number[], value: number, key: "weekdays" | "yearMonths") {
    patch({ [key]: list.includes(value) ? list.filter((item) => item !== value) : [...list, value].sort((a, b) => a - b) });
  }
  function parseMonthDays() {
    const monthDays = [...new Set(monthDaysText.split(/[,，\s]+/).map(Number).filter((day) => Number.isInteger(day) && day >= 1 && day <= 31))].sort((a, b) => a - b);
    patch({ monthDays }); monthDaysText = monthDays.join(", ");
  }
  function apply() {
    if (disabled) return;
    if (draftFrequency === "customMonthly") parseMonthDays();
    change(draftFrequency, Math.min(365, Math.max(1, Math.trunc(draftInterval) || 1)), draftOptions);
    close();
  }

  const untilValue = $derived(draftOptions.endUntilAtUtcMs === null ? "" : new Date(draftOptions.endUntilAtUtcMs - new Date(draftOptions.endUntilAtUtcMs).getTimezoneOffset() * 60_000).toISOString().slice(0, 10));
  const summary = $derived(recurrenceSummary(language, frequency, interval));
</script>

<div class="recurrence-selector" bind:this={anchor}>
  <span>{zh ? "周期重复" : "Recurrence"}</span>
  <Button type="button" variant="secondary" aria-haspopup="dialog" aria-expanded={open} onclick={show} {disabled}>{summary}</Button>
</div>

{#if open}
  <div class="recurrence-popover" bind:this={panel} popover="manual" role="dialog" aria-label={zh ? "设置重复" : "Set recurrence"} style:left={`${left}px`} style:top={`${top}px`} style:max-height={`${maxHeight}px`} data-side={side} oninput={(event) => event.stopPropagation()} onchange={(event) => event.stopPropagation()}>
    <div class="editor">
      <div class="primary-row">
        <label>{zh ? "周期" : "Frequency"}<select value={draftFrequency} onchange={(event) => setFrequency(event.currentTarget.value as Frequency)}>
          <option value="none">{zh ? "不重复" : "Does not repeat"}</option><option value="daily">{zh ? "每天" : "Daily"}</option><option value="weekly">{zh ? "每周" : "Weekly"}</option><option value="monthly">{zh ? "每月" : "Monthly"}</option><option value="yearly">{zh ? "每年" : "Yearly"}</option><option value="customWeekly">{zh ? "自定义星期" : "Custom weekdays"}</option><option value="customMonthly">{zh ? "自定义月日" : "Custom month days"}</option><option value="customYearly">{zh ? "自定义月份" : "Custom months"}</option>
        </select></label>
        <label>{zh ? "间隔" : "Interval"}<input type="number" min="1" max="365" bind:value={draftInterval} disabled={draftFrequency === "none"} /></label>
      </div>
      {#if draftFrequency !== "none"}
        {#if draftFrequency === "customWeekly"}<fieldset><legend>{zh ? "星期" : "Weekdays"}</legend><div class="checks">{#each weekdayLabels as label, index}<label><input type="checkbox" checked={draftOptions.weekdays.includes(index)} onchange={() => toggle(draftOptions.weekdays, index, "weekdays")} />{label}</label>{/each}</div></fieldset>{/if}
        {#if draftFrequency === "customMonthly"}<label>{zh ? "每月日期（逗号分隔）" : "Month days (comma separated)"}<input bind:value={monthDaysText} onblur={parseMonthDays} /></label>{/if}
        {#if draftFrequency === "customYearly"}<fieldset><legend>{zh ? "月份" : "Months"}</legend><div class="checks months">{#each Array.from({ length: 12 }, (_, index) => index + 1) as month}<label><input type="checkbox" checked={draftOptions.yearMonths.includes(month)} onchange={() => toggle(draftOptions.yearMonths, month, "yearMonths")} />{month}</label>{/each}</div></fieldset>{/if}
        <div class="end-row">
          <label>{zh ? "结束方式" : "Ends"}<select value={draftOptions.endKind} onchange={(event) => patch(event.currentTarget.value === "count" ? { endKind: "count", endCount: draftOptions.endCount ?? 100, endUntilAtUtcMs: null } : { endKind: "until", endCount: null, endUntilAtUtcMs: anchorAt ?? Date.now() })}><option value="count">{zh ? "总次数" : "After count"}</option><option value="until">{zh ? "截止日期" : "On date"}</option></select></label>
          {#if draftOptions.endKind === "count"}<label>{zh ? "总次数（含首次）" : "Total (includes first)"}<input type="number" min="1" max="100" value={draftOptions.endCount ?? 100} onchange={(event) => patch({ endCount: Number(event.currentTarget.value) })} /></label>
          {:else}<label>{zh ? "截止日期" : "End date"}<input type="date" value={untilValue} onchange={(event) => patch({ endUntilAtUtcMs: event.currentTarget.value ? new Date(`${event.currentTarget.value}T23:59:59`).getTime() : null })} /></label>{/if}
        </div>
      {/if}
      <div class="actions"><Button type="button" variant="secondary" onclick={close}>{zh ? "取消" : "Cancel"}</Button><Button type="button" onclick={apply}>{zh ? "应用" : "Apply"}</Button></div>
    </div>
  </div>
{/if}

<style>
  .recurrence-popover{--focus-ring:var(--field-focus-ring);position:fixed;inset:auto;margin:0;box-sizing:border-box;width:min(360px,calc(100vw - 16px));max-height:calc(100vh - 16px);overflow:auto;padding:16px;border:0;border-radius:var(--radius-dialog);color:var(--color-text);background:var(--color-surface-raised);box-shadow:var(--shadow-dialog)}
  .recurrence-selector{--focus-ring:var(--field-focus-ring);display:grid;gap:6px}.recurrence-selector>span,label,legend{font-size:var(--font-size-caption);color:var(--color-text-muted)}.recurrence-selector :global(.ui-button){justify-content:flex-start;width:100%}.editor{display:grid;gap:12px}.primary-row,.end-row{display:grid;grid-template-columns:minmax(0,1fr) minmax(92px,.55fr);gap:8px}fieldset{margin:0;padding:0;border:0}.checks{display:flex;flex-wrap:wrap;gap:6px 12px;margin-top:5px}.checks label{display:flex;align-items:center;gap:4px}.months{display:grid;grid-template-columns:repeat(6,minmax(0,1fr))}.actions{display:flex;justify-content:flex-end;gap:8px;margin-top:4px}@media(max-width:420px){.primary-row,.end-row{grid-template-columns:1fr}.months{grid-template-columns:repeat(4,minmax(0,1fr))}}
  .editor{gap:16px}.editor label{display:grid;gap:6px}.primary-row,.end-row{gap:12px}.checks{gap:8px;margin-top:8px}.checks label{display:flex;min-height:36px;padding:4px 8px;border-radius:var(--radius-control);color:var(--color-text);font-size:var(--font-size-meta);cursor:pointer}.checks label:has(:checked){background:var(--color-field-selected)}.checks input{appearance:none;display:grid;place-items:center;flex:0 0 18px;width:18px;height:18px;margin:0;padding:0;border:1px solid var(--color-border-strong);border-radius:4px;background:var(--color-field-idle)}.checks input:checked{background:var(--color-primary);border-color:var(--color-primary)}.checks input:checked::after{content:"";width:5px;height:9px;border:solid var(--color-on-primary);border-width:0 2px 2px 0;transform:translateY(-1px) rotate(45deg)}.checks input:focus-visible{outline:2px solid var(--color-focus);outline-offset:2px}.editor input:not([type="checkbox"]),.editor select{min-width:0;width:100%;min-height:36px;padding:8px;border:0;border-radius:var(--radius-control);font:inherit;font-size:var(--font-size-meta);color:var(--color-text);background:var(--color-field-idle)}.editor input:focus-visible,.editor select:focus-visible{outline:0;box-shadow:var(--focus-ring)}.editor :disabled{color:var(--color-text-muted);background:var(--color-field-disabled);opacity:1}.actions{margin-top:12px}.months{grid-template-columns:repeat(4,minmax(0,1fr))}.recurrence-selector :global(.ui-button.secondary){border:0;background:var(--color-field-idle);color:var(--color-text)}.recurrence-selector :global(.ui-button.secondary:focus-visible){outline:0;box-shadow:var(--focus-ring)}.recurrence-selector :global(.ui-button:disabled){background:var(--color-field-disabled);color:var(--color-text-muted);opacity:1}
</style>
