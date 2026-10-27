export type ReorderAction = "up" | "down" | "top" | "bottom";

export function reorderKeys(
  keys: readonly string[],
  eligibleKeys: readonly string[],
  key: string,
  action: ReorderAction
): string[] {
  const current = keys.indexOf(key);
  const eligible = eligibleKeys.filter((item) => keys.includes(item));
  const position = eligible.indexOf(key);
  if (current < 0 || position < 0) return [...keys];
  const targetKey = action === "up" ? eligible[position - 1]
    : action === "down" ? eligible[position + 1]
      : action === "top" ? eligible[0]
        : eligible[eligible.length - 1];
  if (!targetKey || targetKey === key) return [...keys];
  const next = [...keys];
  next.splice(current, 1);
  const target = next.indexOf(targetKey);
  next.splice(action === "down" || action === "bottom" ? target + 1 : target, 0, key);
  return next;
}
