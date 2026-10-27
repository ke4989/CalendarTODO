/**
 * @template {{ key: string, time: number, end?: number | null }} T
 * @param {readonly T[]} entries
 * @param {number} now
 * @param {(time: number) => boolean} isToday
 * @param {number} [limit]
 * @param {{ todayOnly?: boolean }} [options]
 * @returns {T[]}
 */
export function selectTimelineEntries(entries, now, isToday, limit = 5, options = {}) {
  /** @param {T} entry */
  const ongoing = (entry) => entry.time <= now && entry.end != null && now < entry.end;
  const candidates = entries.filter((entry) => !options.todayOnly || isToday(entry.time) || ongoing(entry));
  // Reserve the bounded window for ongoing and upcoming items before adding
  // recent history. Earlier items today must not crowd out the rest of today.
  const preferred = candidates
    .filter((entry) => entry.time >= now || ongoing(entry))
    .sort((a, b) => a.time - b.time || a.key.localeCompare(b.key))
    .slice(0, limit);
  const nearestHistory = candidates
    .filter((entry) => entry.time < now && !ongoing(entry))
    .sort((a, b) => b.time - a.time || a.key.localeCompare(b.key));
  return [...preferred, ...nearestHistory.slice(0, Math.min(2, limit - preferred.length))]
    .sort((a, b) => a.time - b.time || a.key.localeCompare(b.key));
}

/**
 * Returns the real start shown and sorted on the timeline (due-only tasks use
 * their deadline). Relevance of ongoing intervals is handled by the caller.
 * @param {{ kind: "calendar" | "todo", item: { startAtUtcMs: number | null, dueAtUtcMs?: number | null, endAtUtcMs?: number | null } }} entry
 * @param {number} now
 * @returns {number | null}
 */
export function timelineFocusTime(entry, now) {
  return entry.item.startAtUtcMs ?? (entry.kind === "todo" ? entry.item.dueAtUtcMs ?? null : null);
}

/**
 * One marker, using half-open intervals. Entries are ordered by real start/key.
 * @param {readonly { key: string, time: number, end: number | null }[]} entries
 * @param {number} now
 * @returns {{ key: string | null, placement: "before" | "inside" | "after" }}
 */
export function timelineNowPosition(entries, now) {
  const ongoing = entries.find((entry) => entry.time <= now && entry.end !== null && now < entry.end);
  if (ongoing) return { key: ongoing.key, placement: "inside" };
  const future = entries.find((entry) => entry.time > now);
  if (future) return { key: future.key, placement: "before" };
  return { key: entries.at(-1)?.key ?? null, placement: "after" };
}
