export type CompletionDirection = "complete" | "restore";

const exitTiming = { duration: 160, easing: "cubic-bezier(0.4, 0, 1, 1)", fill: "both" as const };
const moveTiming = { duration: 220, easing: "cubic-bezier(0.16, 1, 0.3, 1)" };
const enterTiming = { duration: 200, delay: 40, easing: "cubic-bezier(0.16, 1, 0.3, 1)", fill: "backwards" as const };

function motionElements(root: ParentNode): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>("[data-completion-motion-id]")];
}

export function reducedCompletionMotion(): boolean {
  return matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export async function animateCompletionChange(
  root: ParentNode,
  fromKey: string,
  toKey: string,
  commit: () => void | Promise<void>,
): Promise<void> {
  if (reducedCompletionMotion()) {
    await commit();
    return;
  }

  const leaving = motionElements(root).filter((element) => element.dataset.completionKey === fromKey);
  const before = new Map(motionElements(root).map((element) => [
    element.dataset.completionMotionId!,
    element.getBoundingClientRect(),
  ]));
  const exitAnimations = leaving.map((element) => element.animate([
    { opacity: 1, transform: "scale(1)" },
    { opacity: 0, transform: "scale(0.84)" },
  ], exitTiming));
  const activeAnimations = [...exitAnimations];
  try {
    await Promise.allSettled(exitAnimations.map((animation) => animation.finished));
    await commit();
    const current = motionElements(root);
    exitAnimations.forEach((animation) => animation.cancel());
    const moveAnimations = current.flatMap((element) => {
      const previous = before.get(element.dataset.completionMotionId!);
      if (!previous) return [];
      const next = element.getBoundingClientRect();
      const x = previous.left - next.left;
      const y = previous.top - next.top;
      return x || y ? [element.animate([
        { transform: `translate(${x}px, ${y}px)` },
        { transform: "translate(0, 0)" },
      ], moveTiming)] : [];
    });
    const enterAnimations = current
      .filter((element) => element.dataset.completionKey === toKey)
      .map((element) => element.animate([
        { opacity: 0, transform: "scale(0.84)" },
        { opacity: 1, transform: "scale(1)" },
      ], enterTiming));
    activeAnimations.push(...moveAnimations, ...enterAnimations);
    await Promise.allSettled([...moveAnimations, ...enterAnimations].map((animation) => animation.finished));
  } finally {
    activeAnimations.forEach((animation) => animation.cancel());
  }
}
