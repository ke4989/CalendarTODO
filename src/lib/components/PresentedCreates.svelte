<script lang="ts">
  import type { PresentedCreate } from "$lib/ipc/contracts";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  import { fitOneLine } from "$lib/ui/fit-one-line";
  import { recurrenceSummary } from "$lib/ui/recurrence";

  let { items, language, recover, surface, showCalendarName, showTime, showTags, formatTime }: {
    items: PresentedCreate[];
    language: ResolvedLanguage;
    recover: (item: PresentedCreate) => void;
    surface: "main" | "desktop";
    showCalendarName: boolean;
    showTime: boolean;
    showTags: boolean;
    formatTime: (item: PresentedCreate) => string;
  } = $props();
</script>

{#snippet content(item: PresentedCreate, recoveryLabel: string | null)}
  <span class="title-line">
    <strong class="title" use:fitOneLine>{item.draft.title}</strong>
    {#if showTags && item.draft.tagNames.length}
      <span class="tags" use:fitOneLine>{item.draft.tagNames.map(name => `#${name}`).join(" ")}</span>
    {/if}
  </span>
  {#if showCalendarName || showTime || surface === "main" && item.draft.important || recoveryLabel}
    <span class="metadata" use:fitOneLine>
      {#if showCalendarName}<b class="calendar-name">{item.calendarName}</b>{/if}
      {#if showTime}
        {@const time = item.draft.startAtUtcMs ?? item.draft.dueAtUtcMs}
        <time datetime={time === null ? undefined : new Date(time).toISOString()}>{formatTime(item)}</time>
      {/if}
      {#if surface === "main" && item.draft.important}<span>{translate(language, "pinned")}</span>{/if}
      {#if recoveryLabel}<span>{recoveryLabel}</span>{/if}
    </span>
  {/if}
  {#if surface === "desktop" && item.draft.recurrence}
    <span class="recurrence" use:fitOneLine>{recurrenceSummary(language, item.draft.recurrence.frequency, item.draft.recurrence.interval)}{item.source === "remote" ? translate(language, "localOnlySuffix") : ""}</span>
  {/if}
{/snippet}

{#each items as item (item.presentationId)}
  {@const recoveryLabel = item.phase === "failed" || item.failure === "localMetadata" ? translate(language, item.failure === "localMetadata" ? "retry" : "recoverCreateDraft") : null}
  <li class="presented-create {surface}" class:compact={!showCalendarName && !showTime && !showTags} data-presentation-id={item.presentationId} style:--calendar-color={item.calendarColor ?? undefined}>
    <div class="row-body">
      <span class="completion-slot" aria-hidden="true"></span>
      {#if recoveryLabel}
        <button class="row-content" type="button" onclick={() => recover(item)} aria-label={`${recoveryLabel} ${item.draft.title}`}>
          {@render content(item, recoveryLabel)}
        </button>
      {:else}
        <div class="row-content">{@render content(item, null)}</div>
      {/if}
      <span class="action-slot" aria-hidden="true"></span>
    </div>
  </li>
{/each}

<style>
  /* Match each incumbent route, whose scoped rules cannot reach this component. */
  .presented-create { min-width: 0; list-style: none; border-bottom: 1px solid var(--color-border); color: var(--color-text); }
  .row-body { min-width: 0; align-items: center; }
  .main .row-body { display: grid; grid-template-columns: 36px minmax(0, 1fr) var(--agenda-action-column, 36px); gap: 12px; min-height: 48px; padding: 4px 2px; }
  .desktop { min-height: 46px; padding: 5px 4px; border-bottom-color: color-mix(in srgb, var(--color-border) 76%, transparent); }
  .desktop.compact { min-height: 38px; padding-block: 3px; }
  .desktop .row-body { display: flex; gap: 10px; }
  .row-content { display: block; flex: 1; min-width: 0; padding: 0; border: 0; background: transparent; color: inherit; font: inherit; text-align: start; }
  .main .row-content { padding: 5px 7px; }
  button.row-content { min-height: 36px; cursor: pointer; }
  button.row-content:focus-visible { border-radius: var(--radius-control); outline: 2px solid var(--color-focus); outline-offset: 1px; }
  .completion-slot { position: relative; display: grid; place-items: center; width: 36px; height: 36px; flex: 0 0 36px; }
  .completion-slot::before { content: ""; width: 18px; height: 18px; border: 1.5px solid var(--calendar-color, var(--color-border-strong)); border-radius: 5px; }
  .desktop .completion-slot { margin-inline: -2px; }
  .action-slot { width: 36px; flex: 0 0 36px; }
  .title-line { display: flex; align-items: baseline; min-width: 0; gap: 12px; line-height: 1.3; }
  .title { min-width: 0; font-size: var(--font-size-caption); font-weight: 600; color: var(--color-text-muted); }
  .tags { flex: 0 1 auto; min-width: 0; font-size: var(--font-size-caption); font-weight: 400; color: var(--color-text-muted); }
  .metadata { display: flex; align-items: baseline; gap: 12px; min-width: 0; color: var(--color-text-muted); font-size: var(--font-size-caption); }
  .metadata > * { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .main .calendar-name { color: var(--calendar-color); }
  .desktop .title-line, .desktop .metadata { gap: 10px; }
  .desktop .title-line { line-height: normal; }
  .desktop .title { color: inherit; font-size: .84rem; font-weight: 700; line-height: 1.25; }
  .desktop .metadata, .recurrence { margin-top: 2px; }
  .recurrence { display: block; min-width: 0; color: var(--color-text-muted); font-size: var(--font-size-caption); }
</style>
