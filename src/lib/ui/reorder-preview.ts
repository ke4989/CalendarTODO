export interface ReorderRow {
  key: string;
  midpointY: number;
}

/** Reorders only the rendered keys while preserving filtered-out persisted
 * positions and appending newly visible keys that have no stored rank yet. */
export function mergeVisibleOrder(
  current: readonly string[],
  fullView: readonly string[],
  visible: readonly string[],
  reordered: readonly string[],
): string[] {
  const rank = new Map(reordered.map((key, index) => [key, index]));
  const visibleSet = new Set(visible);
  const currentSet = new Set(current);
  const order = [...current, ...fullView.filter((key) => !currentSet.has(key))];
  const sortedVisible = order
    .filter((key) => visibleSet.has(key))
    .sort((a, b) => (rank.get(a) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b) ?? Number.MAX_SAFE_INTEGER));
  let index = 0;
  return order.map((key) => visibleSet.has(key) ? sortedVisible[index++] : key);
}

/** Returns the insertion preview for a pointer position, excluding the moving
 * row from hit testing so every slot, including its starting slot, is reachable. */
export function reorderAtPointer(
  keys: readonly string[],
  draggingKey: string,
  pointerY: number,
  rows: readonly ReorderRow[],
): string[] {
  const remaining = keys.filter((key) => key !== draggingKey);
  if (remaining.length === keys.length) return [...keys];
  const positions = new Map(rows.filter((row) => row.key !== draggingKey).map((row) => [row.key, row.midpointY]));
  const insertion = remaining.reduce((index, key) => index + (pointerY > (positions.get(key) ?? Number.POSITIVE_INFINITY) ? 1 : 0), 0);
  remaining.splice(insertion, 0, draggingKey);
  return remaining;
}
