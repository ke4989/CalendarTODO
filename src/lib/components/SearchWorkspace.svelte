<script lang="ts">
  import { onMount, onDestroy, tick, untrack } from "svelte";
  import { translate, type ResolvedLanguage } from "$lib/i18n";
  import { searchRemoteStart, searchRemoteStep, searchRemoteCancel, type RangeView, searchWorkspacePage, searchWorkspacePreferences, type SearchSummary, type SearchPreferences, type SearchField, type SearchCursor, type SearchDirection } from "$lib/ipc/search-workspace";
  let { language, active, excludedResources = [], dataRevision = 0, select, failure }: { language: ResolvedLanguage; active: boolean; excludedResources?: string[]; dataRevision?: unknown; select: (item: SearchSummary) => void; failure: (error: unknown) => void } = $props();
  let text = $state("");
  let preferences = $state<SearchPreferences>({ fields: ["title"], includeCompleted: true });
  let ready = $state(false);
  let initialized = false;
  let pageTemplate: SearchCursor | null = null;
  let scrollIdle: ReturnType<typeof setTimeout> | undefined;
  let scrolling = false;
  let composing = $state(false);
  let rows = $state<SearchSummary[]>([]);
  let before = $state<SearchCursor | null>(null);
  let after = $state<SearchCursor | null>(null);
  let hasBefore = $state(true);
  let hasAfter = $state(true);
  let busy = $state(false);
  let failed = $state(false);
  let anchor = Date.now();
  let anchorKey: string | null = null;
  let generation = 0;
  let view = $state<HTMLDivElement>();
  let input = $state<HTMLInputElement>();
  let scrollTop = $state(0);
  let viewportHeight = $state(500);
  let focusIndex = $state(0);
  let mode = $state<SearchDirection>("after");
  let rangeOpen = $state(false);
  let rangeStart = $state(new Date(Date.now()-365*86400000).toISOString().slice(0,10));
  let rangeEnd = $state(new Date().toISOString().slice(0,10));
  let remote = $state<RangeView | null>(null);
  let remoteGeneration = 0;
  $effect(() => { if (!active && remote?.state === "reading") { remoteGeneration++; remote={...remote,state:"cancelled"}; void searchRemoteCancel().catch(failure); } });
  async function extend() {
    const epoch=++remoteGeneration;
    try {
      remote=await searchRemoteStart({startAtUtcMs:new Date(`${rangeStart}T00:00:00`).getTime(),endAtUtcMs:new Date(`${rangeEnd}T00:00:00`).getTime()});
      while (epoch===remoteGeneration && remote.state==="reading") {
        const next=await searchRemoteStep(remote.taskId);
        if(epoch!==remoteGeneration)return;
        remote=next;await refresh();
      }
      if(remote.state==="failed") failure(new Error("remote search failed"));
    }catch(error){if(epoch===remoteGeneration)failure(error);}
  }
  function preset(future:boolean) {const now=new Date();rangeStart=new Date(now.getTime()+(future?0:-365*86400000)).toISOString().slice(0,10);rangeEnd=new Date(now.getTime()+(future?365*86400000:0)).toISOString().slice(0,10);} 
  let resetPending = false;
  let continueDirection: SearchDirection | null = null;
  let refreshPending = false;
  let locateLatestPast = false;
  const height = 76;
  const visibleRows = $derived(rows.filter(row => row.identity.kind !== "calendar" || !excludedResources.includes(`${row.identity.seriesId}:${row.identity.recurrenceKey}`)));
  const start = $derived(Math.max(0, Math.floor(scrollTop / height) - 7));
  const end = $derived(Math.min(visibleRows.length, Math.ceil((scrollTop + viewportHeight) / height) + 7));
  const windowRows = $derived(visibleRows.slice(start, end));
  onMount(() => () => { generation++; remoteGeneration++; });
  onDestroy(() => { clearTimeout(scrollIdle); if (remote?.state === "reading") void searchRemoteCancel().catch(() => {}); });
  $effect(() => { if(active && !ready) { let disposed=false; void searchWorkspacePreferences().then(value=>{if(!disposed){preferences=value;ready=true;}}).catch(error=>{if(!disposed){ready=true;failure(error);}});return()=>{disposed=true;};} });
  $effect(() => {
    const query = text; const fields = preferences.fields.join(","); const completed = preferences.includeCompleted; const ime = composing;
    if (!ready || ime) return;
    void query; void fields; void completed;
    generation++;
    const timer = setTimeout(() => { initialized = true; void reset(); }, 250);
    return () => clearTimeout(timer);
  });
  $effect(() => { void dataRevision; if (ready && active && initialized) { const timer = setTimeout(() => void untrack(refresh), 180); return () => clearTimeout(timer); } });
  export function focus() { input?.focus(); }
  async function savePreferences() { try { await searchWorkspacePreferences(preferences); } catch(error) { failure(error); } }
  function fieldChanged(field: SearchField, control: HTMLInputElement) {
    const fields = preferences.fields.includes(field) ? preferences.fields.filter(value => value !== field) : [...preferences.fields, field];
    preferences = { ...preferences, fields: fields.length ? fields : [field] };
    // A native checkbox has already toggled. The last field stays selected,
    // so its expression remains true and Svelte need not write the DOM again.
    control.checked = preferences.fields.includes(field);
    void savePreferences();
  }
  async function reset(direction: SearchDirection = "after") {
    generation++; mode = direction; anchor = Date.now(); anchorKey = null; before = null; after = null; pageTemplate = null; locateLatestPast = false; hasBefore = direction !== "noTime"; hasAfter = true; rows = []; focusIndex = 0; scrollTop = 0;
    if (view) view.scrollTop = 0;
    if (busy) { resetPending = true; return; }
    await load(direction, true);
  }
  async function refresh() {
    if (busy || scrolling) { refreshPending = true; return; }
    if (!rows.length) { await load(mode,true); return; }
    // Reload around the visible row, preserving its offset rather than jumping to now.
    const index = Math.min(rows.length - 1, Math.floor(scrollTop / height));
    const item = rows[index]; const offset = scrollTop % height;
    generation++; anchor = item.startAtUtcMs ?? Number.MAX_SAFE_INTEGER; anchorKey = item.key; before = null; after = null; hasBefore = true; hasAfter = true;
    await load(mode === "noTime" ? "noTime" : "after", true);
    await tick();
    const replacement = rows.findIndex(row => row.key === item.key);
    if (view) { view.scrollTop = Math.max(0,replacement) * height + offset; scrollTop = view.scrollTop; }
  }
  async function load(direction: SearchDirection, replace = false) {
    if (busy || (!replace && !(direction === "before" ? hasBefore : hasAfter))) return;
    busy = true; failed = false;
    const epoch = generation;
    try {
      const result = await searchWorkspacePage({ text, preferences, anchorUtcMs: anchor, anchorKey, direction, cursor: replace ? null : direction === "before" ? before : after });
      if (epoch !== generation) return;
      if (result.stale) { refreshPending = true; return; }
      pageTemplate = result.continuation ?? pageTemplate;
      const oldTop = view?.scrollTop ?? 0;
      const first = visibleRows[Math.floor(oldTop / height)]; const offset = oldTop % height;
      const oldKeys = new Set(rows.map(row => row.key));
      const incoming = result.items.filter(row => replace || !oldKeys.has(row.key));
      rows = replace ? incoming : direction === "before" ? [...incoming,...rows] : [...rows,...incoming];
      if (rows.length > 100) {
        if (direction === "before") { rows = rows.slice(0,100); hasAfter = true; after = boundary(rows.at(-1),"after",result); }
        else { rows = rows.slice(-100); hasBefore = true; before = boundary(rows[0],"before",result); }
      }
      if (direction === "before") { before = result.continuation; hasBefore = !result.exhausted; }
      else { after = result.continuation; hasAfter = !result.exhausted; }
      if (replace) { before = boundary(rows[0],"before",result); hasBefore = mode !== "noTime"; }
      await tick();
      if (view && !replace && first) {
        const index = visibleRows.findIndex(row => row.key === first.key);
        if (index >= 0) { view.scrollTop = index * height + offset; scrollTop = view.scrollTop; }
      }
      if (direction === "before" && locateLatestPast && rows.length) { if (view) { view.scrollTop = Math.max(0,view.scrollHeight - view.clientHeight); scrollTop = view.scrollTop; } locateLatestPast = false; }
      if (replace && !rows.length && result.exhausted && direction === "after") { hasBefore = true; locateLatestPast = true; continueDirection = "before"; }
      else if (!result.exhausted && (result.items.length < 25 || (view && view.clientHeight > 0 && view.scrollHeight <= view.clientHeight))) continueDirection = direction;
    } catch(error) { if (epoch === generation) { failed = true; failure(error); } }
    finally {
      busy = false;
      if (resetPending) { resetPending = false; continueDirection = null; void reset(mode); }
      else if (refreshPending && !scrolling) { refreshPending = false; continueDirection = null; void refresh(); }
      else if (continueDirection) { const next = continueDirection; continueDirection = null; setTimeout(() => void load(next),0); }
    }
  }
  function boundary(row: SearchSummary | undefined, direction: SearchDirection, page: { revision: number; continuation: SearchCursor | null }): SearchCursor | null {
    return row && (page.continuation ?? pageTemplate) ? { ...(page.continuation ?? pageTemplate)!,  revision: page.revision, direction, position: { time: row.startAtUtcMs ?? Number.MAX_SAFE_INTEGER, key: row.key } } : null;
  }
  function scroll() {
    if (!view || !active) return; scrollTop = view.scrollTop; viewportHeight = view.clientHeight;
    scrolling = true; clearTimeout(scrollIdle); scrollIdle = setTimeout(() => { scrolling = false; if (refreshPending) { refreshPending = false; void refresh(); } },150);
    if (scrollTop < height * 8 && hasBefore && mode !== "noTime") void load("before");
    else if (view.scrollHeight - scrollTop - viewportHeight < height * 8 && hasAfter) void load(mode === "noTime" ? "noTime" : "after");
  }
  function key(event: KeyboardEvent) {
    if (event.target === input || (event.target instanceof HTMLElement && event.target.closest("input,summary"))) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault(); focusIndex = Math.max(0,Math.min(visibleRows.length - 1,focusIndex + (event.key === "ArrowDown" ? 1 : -1)));
      if (view) { const top = focusIndex * height; if (top < scrollTop || top + height > scrollTop + viewportHeight) view.scrollTop = top; }
    } else if (event.key === "Enter" && visibleRows[focusIndex]) { event.preventDefault(); select(visibleRows[focusIndex]); }
  }
  function date(row: SearchSummary) { return row.startAtUtcMs === null ? translate(language,"searchNoTime") : new Intl.DateTimeFormat(language,{ dateStyle: "medium", timeStyle: "short" }).format(row.startAtUtcMs); }
</script>
<section class="search-workspace" hidden={!active} aria-label={translate(language,"searchWorkspace")}>
  <div class="search-tools">
    <div class="search-field">
      <details>
        <summary>{translate(language,"searchFields")}</summary>
        <div class="field-menu">{#each ["title","notes","location","tags"] as value}<label><input type="checkbox" checked={preferences.fields.includes(value as SearchField)} onchange={event => fieldChanged(value as SearchField, event.currentTarget)} />{translate(language,value as SearchField)}</label>{/each}</div>
      </details>
      <label class="sr-only" for="workspace-search">{translate(language,"searchItems")}</label>
      <input bind:this={input} id="workspace-search" type="search" bind:value={text} placeholder={translate(language,"searchItems")} oncompositionstart={() => composing = true} oncompositionend={() => composing = false} onkeydown={event => { if (event.key === "Escape" && !composing) { event.stopPropagation(); if (text) text = ""; else view?.focus(); } if (event.key === "Enter" && !composing && visibleRows[0]) select(visibleRows[0]); }} />
    </div>
    <div class="search-options"><label><input type="checkbox" bind:checked={preferences.includeCompleted} onchange={savePreferences} />{translate(language,"searchIncludeCompleted")}</label><button type="button" onclick={() => void reset()}>{translate(language,"searchNow")}</button><button type="button" onclick={() => void reset("noTime")}>{translate(language,"searchNoTime")}</button>{#if hasBefore && mode !== "noTime"}<button type="button" disabled={busy} onclick={() => void load("before")}>{translate(language,"searchEarlier")}</button>{/if}</div>
    <div class="remote-controls"><button type="button" onclick={() => rangeOpen = !rangeOpen}>{translate(language,"searchMoreIcloud")}</button>{#if remote?.state === "reading"}<span role="status">{remote.completed} / {remote.total}</span><button type="button" onclick={() => { remoteGeneration++; void searchRemoteCancel().catch(failure); if(remote)remote={...remote,state:"cancelled"}; }}>{translate(language,"searchStop")}</button>{/if}</div>
    {#if rangeOpen}<div class="range-options"><button type="button" onclick={() => preset(false)}>{translate(language,"searchPastYear")}</button><button type="button" onclick={() => preset(true)}>{translate(language,"searchFutureYear")}</button><label>{translate(language,"start")}<input type="date" bind:value={rangeStart} /></label><label>{translate(language,"due")}<input type="date" bind:value={rangeEnd} /></label><button type="button" disabled={remote?.state === "reading" || !rangeStart || !rangeEnd || rangeEnd <= rangeStart} onclick={() => void extend()}>{translate(language,"searchRangeStart")}</button></div>{/if}
    {#if remote}<p class="range-state" role="status">{new Date(remote.range.startAtUtcMs).toLocaleDateString(language)} – {new Date(remote.range.endAtUtcMs).toLocaleDateString(language)} · {translate(language,remote.state === "complete" ? "searchRangeComplete" : remote.state === "cancelled" ? "searchStopped" : remote.state === "failed" ? "searchFailed" : remote.state === "partialCapacity" ? "searchRangePartial" : "searchReading")}{#if remote.unsupported} · {translate(language,"searchUnsupported")}{/if}</p>{/if}
  </div>
  <div class="search-list" bind:this={view} bind:clientHeight={viewportHeight} onscroll={scroll} onkeydown={key} tabindex="0" role="grid" aria-label={translate(language,"searchWorkspace")} aria-busy={busy}>
    <div style:height={`${start * height}px`} aria-hidden="true"></div>
    {#each windowRows as row, index (row.key)}<div class="search-row" role="row" class:completed={row.completed} class:keyboard-selected={focusIndex === start + index}><button type="button" role="gridcell" onclick={() => { focusIndex = start + index; select(row); }}><strong>{row.title}</strong><span><i style:background={row.calendarColor ?? "var(--color-text-muted)"}></i>{row.calendarName}<time>{date(row)}</time>{#if row.completed}<em>{translate(language,"completed")}</em>{/if}</span></button></div>{/each}
    <div style:height={`${Math.max(0,visibleRows.length - end) * height}px`} aria-hidden="true"></div>
    {#if failed}<p role="alert">{translate(language,"searchFailed")} <button type="button" onclick={() => void load(mode)}>{translate(language,"searchRetry")}</button></p>{:else if busy}<p class="edge" role="status">{translate(language,"searchReading")}</p>{:else if !visibleRows.length}<p class="edge">{translate(language,"searchEmpty")}</p>{/if}
    {#if hasAfter}<button class="edge" type="button" disabled={busy} onclick={() => void load(mode === "noTime" ? "noTime" : "after")}>{translate(language,"searchLater")}</button>{/if}
  </div>
</section>
<style>
  .remote-controls,.range-options{display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:8px}.remote-controls button,.range-options button{background:transparent;color:var(--color-text-muted);border:0;min-height:36px}.range-options label{display:flex;align-items:center;gap:6px}.range-state{font-size:13px;color:var(--color-text-muted)}
  .search-workspace{display:flex;flex-direction:column;min-width:0;min-height:0;height:100%;padding:0 24px}.search-workspace[hidden]{display:none}.search-tools{flex:none;padding-bottom:12px}.search-field{display:grid;grid-template-columns:auto minmax(0,1fr);align-items:center;gap:8px}.search-field>input{min-width:0;height:44px;border:0;border-radius:10px;padding:0 12px;color:var(--color-text);background:var(--color-search-input-surface)}.search-field>input:focus-visible{outline:0;box-shadow:inset 0 -2px var(--color-focus)}.search-field details{position:relative}.search-field summary{padding:10px 12px;height:44px;white-space:nowrap;cursor:pointer;border-radius:10px;background:var(--color-search-scope-surface)}.field-menu{position:absolute;left:0;top:100%;z-index:20;background:var(--color-surface-raised);box-shadow:var(--shadow-menu);padding:8px;border-radius:8px;min-width:140px}.field-menu label{display:flex;gap:8px;align-items:center;min-height:36px}.search-options{display:flex;align-items:center;gap:12px;flex-wrap:wrap;color:var(--color-text-muted);font-size:13px;margin-top:8px}.search-options label{display:flex;gap:6px;align-items:center;margin-bottom:0;cursor:pointer}.search-options label,.search-options button{height:36px;min-height:36px;padding:0 10px;border:0;border-radius:8px;background:var(--color-search-option-surface);color:var(--color-text-muted)}.search-options input,.field-menu input{width:18px;height:18px;margin:0;accent-color:var(--color-primary)}.search-options button:hover,.search-options label:hover,.search-field summary:hover{background:var(--color-field-selected)}.edge{background:transparent;color:var(--color-text-muted);border:0;min-height:36px}.search-list{overflow:auto;overflow-anchor:none;min-height:0;flex:1}.search-row{height:76px;box-sizing:border-box}.search-row button{display:flex;flex-direction:column;justify-content:center;gap:8px;text-align:left;width:100%;height:100%;padding:10px 12px;background:transparent;color:var(--color-text);border:0;border-radius:8px}.search-row button:hover,.keyboard-selected button{background:var(--color-surface-soft)}.search-row strong{font-weight:500;max-width:100%;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}.search-row span{display:flex;align-items:center;gap:8px;color:var(--color-text-muted);font-size:13px;white-space:nowrap;max-width:100%;overflow:hidden}.search-row i{width:7px;height:7px;border-radius:50%;flex:none}.search-row time{font-variant-numeric:tabular-nums}.search-row em{font-style:normal}.completed strong{text-decoration:line-through;color:var(--color-text-muted)}.edge{display:block;text-align:center;width:100%;font-size:13px}button:focus-visible,summary:focus-visible{outline:2px solid var(--color-focus);outline-offset:-2px}.sr-only{position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%)}
</style>
