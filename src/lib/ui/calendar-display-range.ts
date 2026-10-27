const DAY_MS = 86_400_000;

/** Presentation-only rolling window. Untimed items remain visible. */
export function withinCalendarDisplayRange(
  startAtUtcMs: number | null,
  endAtUtcMs: number | null,
  pastDays: number | null,
  futureDays: number | null,
  nowUtcMs = Date.now(),
): boolean {
  const start = startAtUtcMs ?? endAtUtcMs;
  const end = endAtUtcMs ?? startAtUtcMs;
  if (start === null || end === null) return true;
  const afterLower = pastDays === null || start >= nowUtcMs - pastDays * DAY_MS || end > nowUtcMs - pastDays * DAY_MS;
  const beforeUpper = futureDays === null || start < nowUtcMs + futureDays * DAY_MS;
  return afterLower && beforeUpper;
}
