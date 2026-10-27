<script lang="ts">
  import { onMount, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { desktopListVisible, getAppSettings, hideTrayMenu, listCalendarSettings, listLocalCalendars, quitFromTray, resizeTrayMenu, setCalendarVisible, setLocalCalendarHidden, showMainFromTray, trayMenuReady, updateAppSettings } from "$lib/ipc/client";
  import type { AppSettings, CalendarSettingsView, LocalCalendar, WindowMode } from "$lib/ipc/contracts";
  import { resolveLanguage, setActiveLanguage, translate } from "$lib/i18n";
  import { applyTheme } from "$lib/theme/apply";

  let settings = $state<AppSettings | null>(null);
  let desktopVisible = $state(false);
  let localCalendars = $state<LocalCalendar[]>([]);
  let remoteCalendars = $state<CalendarSettingsView[]>([]);
  let loading = $state(true);
  let busy = $state(false);
  let loadError = $state("");
  let actionMessage = $state("");
  let appliedHeight = 0;
  let refreshRevision = 0;
  let openGeneration = 0;
  const language = $derived(resolveLanguage(settings?.language ?? "system"));
  $effect(() => { setActiveLanguage(language); document.documentElement.lang = language; });
  $effect(() => { applyTheme(settings?.themePalette ?? "quietDaybook", settings?.colorMode ?? "light", false); });

  async function refresh() {
    // A command plus its broadcast event can start two overlapping reads. Only
    // the newest one may publish state, so older results cannot flash back.
    const revision = ++refreshRevision;
    try {
      const next = await Promise.all([getAppSettings(), desktopListVisible(), listLocalCalendars(), listCalendarSettings()]);
      if (revision !== refreshRevision) return;
      [settings, desktopVisible, localCalendars, remoteCalendars] = next;
      loadError = "";
    } catch { if (revision === refreshRevision) loadError = translate(language, "trayStateReadFailed"); }
    finally { if (revision === refreshRevision) loading = false; }
  }

  async function fitWindow() {
    await tick();
    const card = document.querySelector<HTMLElement>(".tray-card");
    if (!card) return;
    const style = getComputedStyle(card);
    const desired = Math.ceil(card.scrollHeight + parseFloat(style.borderTopWidth) + parseFloat(style.borderBottomWidth));
    if (desired <= 0 || desired === appliedHeight) return;
    appliedHeight = desired;
    try { await resizeTrayMenu(desired); }
    catch { appliedHeight = 0; }
  }

  async function prepareOpen(generation: number) {
    openGeneration = generation;
    await refresh();
    await fitWindow();
    if (openGeneration === generation) await trayMenuReady(generation);
  }

  onMount(() => {
    let disposed = false;
    const stops: (() => void)[] = [];
    const observer = new ResizeObserver(() => { void fitWindow(); });
    for (const name of ["calendar-todo://tray-menu-state-changed", "calendar-todo://settings-changed", "calendar-todo://local-todo-changed", "calendar-todo://agenda-data-changed"]) {
      void listen(name, () => { void refresh().then(() => fitWindow()); }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    }
    void listen<number>("calendar-todo://tray-menu-open-check", (event) => { void prepareOpen(event.payload); }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    const card = document.querySelector<HTMLElement>(".tray-card");
    if (card) observer.observe(card);
    void refresh().then(() => fitWindow());
    return () => { disposed = true; observer.disconnect(); stops.forEach((stop) => stop()); };
  });

  async function changeSettings(changes: Partial<Pick<AppSettings, "windowMode" | "desktopListMode">>) {
    if (!settings || busy) return;
    busy = true; actionMessage = "";
    try {
      settings = await updateAppSettings({
        language: settings.language, themePalette: settings.themePalette, colorMode: settings.colorMode, windowMode: changes.windowMode ?? settings.windowMode,
        desktopListMode: changes.desktopListMode ?? settings.desktopListMode,
        syncIntervalMinutes: settings.syncIntervalMinutes, syncOnStartup: settings.syncOnStartup,
        desktopOpacityPercent: settings.desktopOpacityPercent,
        desktopShowCalendarName: settings.desktopShowCalendarName, desktopShowTime: settings.desktopShowTime, desktopShowTags: settings.desktopShowTags,
        mainShowCalendarName: settings.mainShowCalendarName, mainShowTime: settings.mainShowTime, mainShowTags: settings.mainShowTags,
        startHiddenInTray: settings.startHiddenInTray, launchAtLogin: settings.launchAtLogin,
        localTodoColor: settings.localTodoColor, localDisplayPastDays: settings.localDisplayPastDays,
        localDisplayFutureDays: settings.localDisplayFutureDays, defaultCalendarId: settings.defaultCalendarId, reminderNotificationsEnabled: settings.reminderNotificationsEnabled
      });
      desktopVisible = await desktopListVisible();
      await fitWindow();
    } catch { actionMessage = translate(language, "settingsSaveFailed"); }
    finally { busy = false; }
  }

  async function toggleLocal(calendar: LocalCalendar) {
    if (busy) return;
    busy = true; actionMessage = "";
    try { await setLocalCalendarHidden({ calendarId: calendar.calendarId, hidden: !calendar.hidden }); }
    catch { actionMessage = translate(language, "calendarSelectionFailed"); }
    finally { busy = false; }
  }

  async function toggleRemote(calendar: CalendarSettingsView) {
    if (busy) return;
    busy = true; actionMessage = "";
    try { await setCalendarVisible(calendar.accountId, calendar.calendarId, !calendar.visible); }
    catch { actionMessage = translate(language, "calendarSelectionFailed"); }
    finally { busy = false; }
  }
</script>

<svelte:head><title>{translate(language, "trayMenu")}</title></svelte:head>
<main class="tray-card" aria-label={translate(language, "trayMenu")} aria-busy={loading || busy}>
  <div class="menu-title">Calendar TODO <button type="button" class="close" aria-label={translate(language, "closeMenu")} onclick={() => void hideTrayMenu()}><svg viewBox="0 0 20 20" aria-hidden="true"><path d="m5 5 10 10M15 5 5 15" /></svg></button></div>
  {#if loading}<p class="loading" role="status">{translate(language, "trayLoading")}</p>{:else}
  <button type="button" onclick={() => void showMainFromTray()}>{translate(language, "showMain")}</button>
  <button type="button" class:busy-disabled={busy} disabled={!settings || busy} onclick={() => void changeSettings({ desktopListMode: !desktopVisible })}>{desktopVisible ? translate(language, "hideDesktop") : translate(language, "showDesktop")}</button>
  <details ontoggle={() => void fitWindow()}>
    <summary>{translate(language, "desktopMode")}<svg class="chevron" viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" /></svg></summary>
    <div class="submenu" role="group" aria-label={translate(language, "desktopMode")}>{#each [["desktopCompanion", translate(language, "companion")], ["alwaysOnTop", translate(language, "topmost")]] as option}<button type="button" class:busy-disabled={busy} disabled={!settings || busy} aria-pressed={settings?.windowMode === option[0]} onclick={() => void changeSettings({ windowMode: option[0] as WindowMode })}><span class="mark">{#if settings?.windowMode === option[0]}<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8 3 3 7-7" /></svg>{/if}</span><span>{option[1]}</span></button>{/each}</div>
  </details>
  <details ontoggle={() => void fitWindow()}>
    <summary>{translate(language, "chooseDisplayedCalendars")}<svg class="chevron" viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" /></svg></summary>
    <div class="submenu calendars" role="group" aria-label={translate(language, "chooseDisplayedCalendars")}>
      {#each remoteCalendars.filter((calendar) => calendar.available) as calendar (calendar.calendarId)}<button type="button" class:busy-disabled={busy} disabled={busy} aria-pressed={calendar.visible} title={calendar.displayName} onclick={() => void toggleRemote(calendar)}><span class="calendar-dot" class:off={!calendar.visible} style:--calendar-color={calendar.effectiveColor ?? "#2F7D68"}></span><span class="calendar-name">{calendar.displayName}</span></button>{/each}
      {#each localCalendars as calendar (calendar.calendarId)}<button type="button" class:busy-disabled={busy} disabled={busy} aria-pressed={!calendar.hidden} title={calendar.name} onclick={() => void toggleLocal(calendar)}><span class="calendar-dot" class:off={calendar.hidden} style:--calendar-color={calendar.color}></span><span class="calendar-name">{calendar.name}</span></button>{/each}
    </div>
  </details>
  <button type="button" class="quit" onclick={() => void quitFromTray()}>{translate(language, "quit")}</button>
  {/if}
  {#if loadError}<p class="menu-message" role="alert">{loadError}</p>{/if}
  {#if actionMessage}<p class="menu-message" role="alert">{actionMessage}</p>{/if}
</main>

<style>
  :global(*),:global(*::before),:global(*::after){box-sizing:border-box}
  :global(html),:global(body){width:100%;height:100%;margin:0;overflow:hidden;background:transparent;font-family:var(--font-ui)}
  .tray-card{width:100%;height:auto;max-height:100%;padding:6px;overflow-y:auto;border:1px solid var(--color-border);border-radius:12px;background:var(--color-surface);color:var(--color-text)}
  .menu-title{min-height:29px;padding:0 2px 2px 8px;display:flex;align-items:center;justify-content:space-between;color:var(--color-text-muted);font-size:var(--font-size-caption);font-weight:700}.menu-title .close{width:36px;height:36px;min-height:36px;justify-content:center;padding:0}.close svg{width:16px;height:16px;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round}
  button,summary{width:100%;min-height:36px;display:flex;align-items:center;gap:6px;padding:6px 8px;border:0;border-radius:7px;background:transparent;color:inherit;font:inherit;font-size:.79rem;text-align:left;cursor:pointer;list-style:none}
  summary::-webkit-details-marker{display:none}.chevron{width:16px;height:16px;margin-left:auto;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round;stroke-linejoin:round;transition:transform var(--duration-fast) var(--ease-standard)}details[open]>summary .chevron{transform:rotate(90deg)}button:hover,summary:hover{background:var(--color-surface-soft)}button:focus-visible,summary:focus-visible{outline:2px solid var(--color-focus);outline-offset:-2px}button:disabled{opacity:.5;cursor:default}button.busy-disabled:disabled{opacity:1}
  .submenu{margin:0 0 2px 8px;padding-left:5px;border-left:1px solid var(--color-border)}.submenu button{min-height:36px}.mark{width:16px;flex:0 0 16px;color:var(--color-primary)}.mark svg{width:15px;height:15px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}.calendar-dot{width:10px;height:10px;flex:0 0 10px;border:1.5px solid var(--calendar-color);border-radius:50%;background:var(--calendar-color)}.calendar-dot.off{background:transparent}.calendar-name{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.loading{margin:5px 8px;color:var(--color-text-muted);font-size:var(--font-size-caption)}.quit{margin-top:3px;border-top:1px solid var(--color-border);border-radius:0}.menu-message{margin:4px 7px;color:var(--color-danger);font-size:var(--font-size-caption);line-height:1.3}
</style>
