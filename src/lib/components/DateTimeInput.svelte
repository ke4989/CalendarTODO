<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import IconButton from "$lib/components/IconButton.svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  import { localDateKey, localPickerNow, offsetPickerDay, offsetPickerMonth, offsetPickerTimeUnit, pickerCalendarDays, pickerDateTime } from "$lib/ui/date-time-picker";

  type Props = {
    id: string;
    value: string;
    onValue: (value: string) => void;
    step?: number | string;
    min?: string;
    disabled?: boolean;
    language?: ResolvedLanguage;
  };

  let { id, value, onValue, step, min, disabled = false, language = "zh-CN" }: Props = $props();
  let trigger = $state<HTMLSpanElement>();
  let picker = $state<HTMLDivElement>();
  let open = $state(false);
  let compact = $state(false);
  let stage = $state<"date" | "time">("date");
  let observer: ResizeObserver | undefined;
  let layoutFrame: number | undefined;
  let draftDate = $state("");
  let draftTime = $state<{ hour: number | undefined; minute: number | undefined; second: number | undefined }>({ hour: 0, minute: 0, second: 0 });
  let month = $state("");
  let focusedDate = $state("");
  let todayKey = $state("");
  const showSeconds = $derived(Number(step ?? 60) < 60);
  const timeFields = $derived([
    { key: "hour" as const, max: 23 }, { key: "minute" as const, max: 59 },
    ...(showSeconds ? [{ key: "second" as const, max: 59 }] : []),
  ]);
  const days = $derived(month ? pickerCalendarDays(month) : []);
  const weekdays = $derived(Array.from({ length: 7 }, (_, index) => ({
    short: new Intl.DateTimeFormat(language, { weekday: "narrow" }).format(new Date(2026, 9, 19 + index, 12)),
    full: new Intl.DateTimeFormat(language, { weekday: "long" }).format(new Date(2026, 9, 19 + index, 12)),
  })));
  const monthLabel = $derived(month ? new Intl.DateTimeFormat(language, { year: "numeric", month: "long" }).format(new Date(`${month}-01T12:00:00`)) : "");
  const draftValue = $derived(showSeconds && draftTime.second === undefined ? "" : pickerDateTime(draftDate, draftTime.hour, draftTime.minute, showSeconds ? draftTime.second : undefined));
  const canApply = $derived(!!draftValue && (!min || `${draftValue}:00`.slice(0, 19) >= `${min}:00`.slice(0, 19)));
  const canProceed = $derived(compact && stage === "date" ? !!draftDate && (!min || draftDate >= min.slice(0, 10)) : canApply);
  let left = $state(0);
  let top = $state(0);

  $effect(() => { if (disabled) open = false; });
  $effect(() => { if (!open) observer?.disconnect(); });

  function splitValue(source: string) {
    const [date = "", time = ""] = source.split("T");
    draftDate = date;
    const [hour = "0", minute = "0", second = "0"] = time.split(":");
    draftTime = { hour: Number(hour), minute: Number(minute), second: Number(second) };
    month = date.slice(0, 7);
    focusedDate = date;
  }

  async function positionPicker() {
    if (!open || !trigger || !picker) return;
    const viewport = window.visualViewport;
    const viewportLeft = viewport?.offsetLeft ?? 0, viewportTop = viewport?.offsetTop ?? 0;
    const viewportWidth = viewport?.width ?? document.documentElement.clientWidth;
    const viewportHeight = viewport?.height ?? document.documentElement.clientHeight;
    const railRight = document.querySelector<HTMLElement>("[data-navigation-rail]")?.getBoundingClientRect().right ?? 0;
    const minLeft = Math.max(viewportLeft + 8, Math.ceil(railRight) + 8);
    const availableWidth = Math.max(0, viewportLeft + viewportWidth - minLeft - 8);
    const availableHeight = Math.max(0, viewportHeight - 16);
    picker.style.maxWidth = `${availableWidth}px`;
    picker.style.maxHeight = `${availableHeight}px`;
    const focusedTime = picker.querySelector(".picker-time")?.contains(document.activeElement);
    const content = picker.querySelector<HTMLElement>(".picker-content")!;
    const panelScrollTop = picker.scrollTop;
    const contentScrollTop = content.scrollTop;
    picker.classList.add("measuring");
    const padding = parseFloat(getComputedStyle(picker).paddingLeft) + parseFloat(getComputedStyle(picker).paddingRight);
    const wideWidth = content.scrollWidth + padding;
    const wideHeight = picker.getBoundingClientRect().height;
    picker.classList.remove("measuring");
    picker.scrollTop = panelScrollTop;
    content.scrollTop = contentScrollTop;
    picker.style.setProperty("--expanded-height", `${wideHeight}px`);
    const nextCompact = wideWidth > availableWidth || wideHeight > availableHeight;
    if (nextCompact && !compact) stage = focusedTime ? "time" : "date";
    compact = nextCompact;
    await tick();
    if (!open || !trigger || !picker) return;
    picker.scrollTop = panelScrollTop;
    content.scrollTop = contentScrollTop;
    const anchor = trigger.getBoundingClientRect();
    const rect = picker.getBoundingClientRect();
    const maxLeft = Math.max(minLeft, viewportLeft + viewportWidth - rect.width - 8);
    left = Math.max(minLeft, Math.min(anchor.right - rect.width, maxLeft));
    const minTop = viewportTop + 8;
    const below = anchor.bottom + 6;
    const above = anchor.top - rect.height - 6;
    const maxTop = Math.max(minTop, viewportTop + viewportHeight - rect.height - 8);
    top = below + rect.height <= viewportTop + viewportHeight - 8 ? below : above >= minTop ? above : Math.min(maxTop, Math.max(minTop, below));
  }

  function scheduleLayout(event?: Event | ResizeObserverEntry[]) {
    if (!open || layoutFrame !== undefined) return;
    if (event instanceof Event && event.type === "scroll" && event.target instanceof Node && picker?.contains(event.target)) return;
    layoutFrame = requestAnimationFrame(() => { layoutFrame = undefined; void positionPicker(); });
  }

  function focusDate() {
    (picker?.querySelector<HTMLButtonElement>(`[data-date="${focusedDate}"]:not(:disabled)`) ?? picker?.querySelector<HTMLButtonElement>(".calendar-day:not(:disabled)"))?.focus();
  }

  async function setStage(next: "date" | "time") {
    stage = next;
    await tick();
    if (next === "date") focusDate();
    else picker?.querySelector<HTMLInputElement>(".picker-time input")?.focus();
  }

  async function openPicker() {
    if (disabled) return;
    const now = new Date();
    todayKey = localDateKey(now);
    splitValue(value || localPickerNow(now, showSeconds));
    compact = false;
    stage = "date";
    open = true;
    await tick();
    await positionPicker();
    if (!open || !trigger || !picker) return;
    observer?.observe(picker);
    observer?.observe(picker.querySelector<HTMLElement>(".picker-actions")!);
    observer?.observe(trigger);
    focusDate();
  }

  function closePicker() {
    open = false;
    trigger?.querySelector<HTMLButtonElement>("button")?.focus();
  }

  function apply() {
    if (!canProceed) return;
    if (compact && stage === "date") { void setStage("time"); return; }
    onValue(draftValue);
    closePicker();
  }

  function changeMonth(amount: number) {
    month = offsetPickerMonth(month, amount);
    focusedDate = draftDate.startsWith(month) ? draftDate : `${month}-01`;
  }

  async function calendarKey(event: KeyboardEvent, date: string) {
    const offsets: Record<string, number> = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 };
    if (!(event.key in offsets) && event.key !== "Home" && event.key !== "End" && event.key !== "PageUp" && event.key !== "PageDown") return;
    event.preventDefault(); event.stopPropagation();
    if (event.key === "PageUp" || event.key === "PageDown") {
      changeMonth(event.key === "PageUp" ? -1 : 1);
    } else {
      const weekday = (new Date(`${date}T12:00:00`).getDay() + 6) % 7;
      const next = offsetPickerDay(date, event.key === "Home" ? -weekday : event.key === "End" ? 6 - weekday : offsets[event.key]);
      focusedDate = min && next < min.slice(0, 10) ? min.slice(0, 10) : next;
      month = focusedDate.slice(0, 7);
    }
    await tick();
    picker?.querySelector<HTMLButtonElement>(`[data-date="${focusedDate}"]`)?.focus();
  }

  function adjustTime(key: "hour" | "minute" | "second", amount: number, max: number) {
    draftTime[key] = offsetPickerTimeUnit(draftTime[key], amount, max);
  }

  function timeLabel(key: "hour" | "minute" | "second", amount: number, max: number) {
    return String(offsetPickerTimeUnit(draftTime[key], amount, max)).padStart(2, "0");
  }

  function wheelTime(event: WheelEvent, key: "hour" | "minute" | "second", max: number) {
    event.preventDefault();
    event.stopPropagation();
    if (event.deltaY !== 0) adjustTime(key, event.deltaY > 0 ? 1 : -1, max);
  }

  function timeKey(event: KeyboardEvent, key: "hour" | "minute" | "second", max: number) {
    const amounts: Record<string, number> = { ArrowUp: 1, ArrowDown: -1, PageUp: 5, PageDown: -5 };
    if (event.key in amounts) {
      event.preventDefault();
      event.stopPropagation();
      adjustTime(key, amounts[event.key], max);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      event.stopPropagation();
      draftTime[key] = event.key === "Home" ? 0 : max;
    }
  }

  function pointerDown(event: PointerEvent) {
    if (open && picker && trigger && !picker.contains(event.target as Node) && !trigger.contains(event.target as Node)) open = false;
  }

  function keyDown(event: KeyboardEvent) {
    if (open && event.key === "Escape") {
      event.preventDefault();
      event.stopImmediatePropagation();
      closePicker();
    } else if (open && event.key === "Tab" && picker) {
      const controls = [...picker.querySelectorAll<HTMLElement>('button:not(:disabled):not([tabindex="-1"]), input:not(:disabled)')].filter((element) => element.getClientRects().length);
      if (controls.length) {
        event.preventDefault(); event.stopImmediatePropagation();
        const index = controls.indexOf(document.activeElement as HTMLElement);
        controls[(index + (event.shiftKey ? -1 : 1) + controls.length) % controls.length].focus();
      }
    } else if (open && event.key === "Enter" && event.target instanceof HTMLInputElement && picker?.contains(event.target)) {
      event.preventDefault(); event.stopImmediatePropagation(); apply();
    }
  }

  onMount(() => {
    document.addEventListener("pointerdown", pointerDown);
    document.addEventListener("keydown", keyDown, true);
    window.addEventListener("resize", scheduleLayout);
    window.visualViewport?.addEventListener("resize", scheduleLayout);
    window.visualViewport?.addEventListener("scroll", scheduleLayout);
    document.addEventListener("scroll", scheduleLayout, true);
    observer = new ResizeObserver(scheduleLayout);
    return () => observer?.disconnect();
  });
  onDestroy(() => {
    document.removeEventListener("pointerdown", pointerDown);
    document.removeEventListener("keydown", keyDown, true);
    window.removeEventListener("resize", scheduleLayout);
    window.visualViewport?.removeEventListener("resize", scheduleLayout);
    window.visualViewport?.removeEventListener("scroll", scheduleLayout);
    document.removeEventListener("scroll", scheduleLayout, true);
    if (layoutFrame !== undefined) cancelAnimationFrame(layoutFrame);
  });
</script>

<div class="date-time-input">
  <input
    {id}
    type="datetime-local"
    {value}
    {step}
    {min}
    {disabled}
    autocomplete="off"
    oninput={(event) => onValue(event.currentTarget.value)}
  />
  <span bind:this={trigger} class="picker-trigger"><IconButton
    variant="quiet"
    type="button"
    label={translate(language, "showPicker")}
    title={translate(language, "showPicker")}
    aria-expanded={open}
    aria-controls={`${id}-picker`}
    {disabled}
    onclick={() => open ? closePicker() : void openPicker()}
  >
    <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 3v3M17 3v3M4 9h16M20 10V6a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v13a1 1 0 0 0 1 1h5" /><circle cx="16.5" cy="16.5" r="4.5" /><path d="M16.5 14v2.5l1.5 1" /></svg>
  </IconButton></span>
</div>

{#if open}
  <div bind:this={picker} id={`${id}-picker`} class="date-time-picker" class:compact data-layout={compact ? "compact" : "wide"} data-step={compact ? stage : "both"} role="dialog" aria-label={translate(language, "showPicker")} style:left={`${left}px`} style:top={`${top}px`}>
    <div class="picker-content">
      <section class="picker-calendar" hidden={compact && stage !== "date"}>
        <header class="month-heading">
          <IconButton type="button" size="compact" label={translate(language, "previousMonth")} onclick={() => changeMonth(-1)}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m14 6-6 6 6 6" /></svg></IconButton>
          <strong aria-live="polite">{monthLabel}</strong>
          <IconButton type="button" size="compact" label={translate(language, "nextMonth")} onclick={() => changeMonth(1)}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m10 6 6 6-6 6" /></svg></IconButton>
        </header>
        <div class="month-grid" role="group" aria-label={monthLabel}>
          {#each weekdays as weekday}<span class="weekday" title={weekday.full}>{weekday.short}</span>{/each}
          {#each days as date}
            <button class="calendar-day" class:other-month={!date.startsWith(month)} class:selected={date === draftDate} type="button" data-date={date} aria-label={new Intl.DateTimeFormat(language, { dateStyle: "full" }).format(new Date(`${date}T12:00:00`))} aria-pressed={date === draftDate} aria-current={date === todayKey ? "date" : undefined} tabindex={date === focusedDate ? 0 : -1} disabled={!!min && date < min.slice(0, 10)} onkeydown={(event) => void calendarKey(event, date)} onclick={() => { draftDate = date; focusedDate = date; month = date.slice(0, 7); }}>{Number(date.slice(-2))}</button>
          {/each}
        </div>
      </section>
      <section class="picker-time" hidden={compact && stage !== "time"} class:with-seconds={showSeconds} aria-label={translate(language, "time")}>
        {#each timeFields as field}
          <div class="time-part" onwheel={(event) => wheelTime(event, field.key, field.max)}>
            <span class="time-label">{translate(language, field.key)}</span>
            <IconButton type="button" size="compact" label={`${translate(language, "increase")} ${translate(language, field.key)}`} onclick={() => adjustTime(field.key, 1, field.max)}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 14 5-5 5 5" /></svg></IconButton>
            <label class="time-wheel">
              <span class="wheel-value" aria-hidden="true">{timeLabel(field.key, -1, field.max)}</span>
              <input type="number" min="0" max={field.max} step="1" bind:value={draftTime[field.key]} onfocus={(event) => event.currentTarget.select()} onkeydown={(event) => timeKey(event, field.key, field.max)} aria-label={translate(language, field.key)} />
              <span class="wheel-value" aria-hidden="true">{timeLabel(field.key, 1, field.max)}</span>
            </label>
            <IconButton type="button" size="compact" label={`${translate(language, "decrease")} ${translate(language, field.key)}`} onclick={() => adjustTime(field.key, -1, field.max)}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 10 5 5 5-5" /></svg></IconButton>
          </div>
        {/each}
      </section>
    </div>
    <div class="picker-actions">{#if compact && stage === "time"}<button class="back" type="button" onclick={() => void setStage("date")}>{translate(language, "backToDate")}</button>{/if}<button type="button" disabled={!canProceed} onclick={apply}>{translate(language, "done")}</button></div>
  </div>
{/if}

<style>
  .date-time-input{position:relative;display:flex;min-width:0}
  .date-time-input>input{min-width:0;width:100%;min-height:var(--control-height);padding:7px 46px 7px 10px;border:1px solid var(--color-border);border-radius:var(--radius-control);color:inherit;background:var(--color-surface);font:inherit}
  .date-time-input>input::-webkit-calendar-picker-indicator{display:none}
  .date-time-input>input::-webkit-datetime-edit-year-field:focus,.date-time-input>input::-webkit-datetime-edit-month-field:focus,.date-time-input>input::-webkit-datetime-edit-day-field:focus,.date-time-input>input::-webkit-datetime-edit-hour-field:focus,.date-time-input>input::-webkit-datetime-edit-minute-field:focus,.date-time-input>input::-webkit-datetime-edit-second-field:focus,.date-time-input>input::-webkit-datetime-edit-ampm-field:focus{color:var(--color-text);background:var(--color-selection);border-radius:3px}
  .picker-trigger{position:absolute;right:2px;top:2px;bottom:2px;display:flex}.picker-trigger :global(.icon-button){width:40px;height:100%;min-width:40px;min-height:0;border-left:1px solid var(--color-border);border-radius:0 calc(var(--radius-control) - 1px) calc(var(--radius-control) - 1px) 0}
  svg{width:19px;height:19px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}
  .date-time-picker{position:fixed;box-sizing:border-box;z-index:80;width:min(480px,calc(100vw - 16px));max-width:calc(100vw - 16px);max-height:calc(100vh - 16px);padding:12px;display:flex;flex-direction:column;gap:8px;border:0;border-radius:var(--radius-dialog);color:var(--color-text);background:var(--color-surface-raised);box-shadow:var(--shadow-dialog)}
  .picker-content{flex:1 1 auto;min-height:0;overflow-y:auto;min-width:0;display:grid;grid-template-columns:minmax(264px,1.55fr) minmax(133px,.85fr);align-items:stretch;gap:12px}.picker-calendar{min-width:0}
  .month-heading{display:flex;align-items:center;justify-content:space-between;gap:4px}.month-heading strong{min-width:0;font-size:var(--font-size-meta);font-weight:600;line-height:1.2;text-align:center}
  .month-grid{display:grid;grid-template-columns:repeat(7,minmax(0,1fr));gap:2px}.weekday{height:24px;display:grid;place-items:center;color:var(--color-text-muted);font-size:var(--font-size-caption)}
  .calendar-day{min-width:0;height:36px;padding:0;border:0;border-radius:var(--radius-control);color:var(--color-text);background:transparent;font:inherit;font-size:var(--font-size-meta);font-variant-numeric:tabular-nums;cursor:pointer}
  .calendar-day.other-month{color:var(--color-text-muted)}.calendar-day:hover:not(:disabled){background:var(--color-surface-soft)}.calendar-day[aria-current="date"]{text-decoration:underline;text-underline-offset:4px}.calendar-day.selected,.calendar-day.selected:hover{color:var(--color-on-primary);background:var(--color-primary)}.calendar-day:disabled{opacity:.35;cursor:default}
  .picker-time{min-width:0;padding-left:12px;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));align-items:center;gap:6px;border-left:1px solid var(--color-border)}.picker-time.with-seconds{grid-template-columns:repeat(3,minmax(0,1fr))}.time-part{min-width:0;display:grid;justify-items:stretch;gap:3px}.time-label{min-width:0;color:var(--color-text-muted);font-size:var(--font-size-caption);text-align:center;overflow:hidden;text-overflow:ellipsis}.time-part :global(.icon-button){height:36px;min-height:36px;width:100%;min-width:36px}
  .time-wheel{width:100%;height:104px;margin:0;display:grid;grid-template-rows:30px 44px 30px;align-items:center;overflow:hidden;border-radius:var(--radius-control);color:var(--color-text-muted);background:var(--color-surface-soft);font-variant-numeric:tabular-nums;text-align:center}.wheel-value{font-size:var(--font-size-caption);opacity:.58;user-select:none}.time-part input{width:100%;min-width:0;min-height:44px;padding:5px 2px;border:0;border-radius:0;color:var(--color-text);background:var(--color-field-idle);font:inherit;font-size:var(--font-size-body);font-variant-numeric:tabular-nums;text-align:center;box-shadow:inset 0 1px var(--color-border),inset 0 -1px var(--color-border);appearance:textfield}.time-part input::-webkit-inner-spin-button,.time-part input::-webkit-outer-spin-button{appearance:none;margin:0}.time-part input:focus{outline:2px solid var(--ring);outline-offset:-3px;background:var(--color-field-idle);box-shadow:none}
  .picker-actions{flex:0 0 auto;display:flex;justify-content:flex-end;gap:8px;padding-top:2px}
  .picker-actions button{min-width:64px;min-height:36px;padding:7px 14px;border:0;border-radius:var(--radius-control);color:var(--color-on-primary);background:var(--color-primary);font:inherit;font-weight:650}
  .picker-actions button:hover:not(:disabled){background:var(--color-primary-hover)}
  .picker-actions .back{margin-right:auto;background:var(--color-field-idle);color:var(--color-text)}
  .date-time-picker.compact{width:320px;height:var(--expanded-height)}
  .compact .picker-content{grid-template-columns:minmax(0,1fr)}
  .compact .picker-time{padding-left:0;border-left:0}
  .picker-calendar[hidden],.picker-time[hidden]{display:none}
  .date-time-picker:global(.measuring){width:480px;height:auto;max-height:none!important}
  :global(.measuring) .picker-content{flex:none;overflow:visible;grid-template-columns:minmax(264px,1.55fr) minmax(133px,.85fr)}
  :global(.measuring) .picker-calendar{display:block}
  :global(.measuring) .picker-time{display:grid;padding-left:12px;border-left:1px solid var(--color-border)}
  @media(max-height:160px){.date-time-picker{overflow-y:auto;padding:4px}.picker-content{flex:none;overflow:visible}.picker-actions{flex-shrink:0}}
</style>
