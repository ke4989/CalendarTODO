/**
 * Keeps one line of text readable at a variable width.
 *
 * Text that already fits keeps its original font size; only text that would not
 * fit on one line is shrunk, and never below `minScale`, after which the ellipsis
 * takes over. This replaces viewport-based `clamp(…vw…)` sizing, which shrank
 * every row as soon as the window narrowed.
 *
 * Usage: `<p use:fitOneLine>…</p>` with a fixed base font size in CSS.
 */
export interface FitOneLineOptions {
  /** Lowest shrink factor applied before falling back to the ellipsis. */
  minScale?: number;
}

const DEFAULT_MIN_SCALE = 0.72;

export function fitOneLine(node: HTMLElement, options: FitOneLineOptions = {}) {
  let minScale = options.minScale ?? DEFAULT_MIN_SCALE;
  let base = 0;
  let lastWidth = -1;
  let frame = 0;

  // The action owns the single-line contract: measuring a natural width needs
  // `nowrap`, and the ellipsis keeps the row from overflowing afterwards.
  const previous = {
    whiteSpace: node.style.whiteSpace,
    overflow: node.style.overflow,
    textOverflow: node.style.textOverflow
  };
  node.style.whiteSpace = "nowrap";
  node.style.overflow = "hidden";
  node.style.textOverflow = "ellipsis";

  function baseFontSize(): number {
    if (base) return base;
    node.style.fontSize = "";
    const computed = Number.parseFloat(getComputedStyle(node).fontSize);
    if (Number.isFinite(computed) && computed > 0) base = computed;
    return base;
  }

  function fit() {
    frame = 0;
    const baseSize = baseFontSize();
    const available = node.clientWidth;
    lastWidth = available;
    if (!baseSize || available <= 0) return;
    node.style.fontSize = "";
    // Two passes at most: shrinking changes the measured width, so one extra
    // pass removes the last pixel of overflow without over-shrinking.
    for (let pass = 0; pass < 2; pass += 1) {
      const natural = node.scrollWidth;
      if (natural <= available) return;
      const current = Number.parseFloat(getComputedStyle(node).fontSize) || baseSize;
      const next = Math.max(baseSize * minScale, current * (available / natural));
      node.style.fontSize = `${next.toFixed(2)}px`;
      if (next <= baseSize * minScale + 0.05) return;
    }
  }

  function schedule() {
    if (!frame) frame = requestAnimationFrame(fit);
  }

  // Only a width change needs a refit: the height also changes when this action
  // resizes the text, and reacting to that would refit in a loop.
  function scheduleForWidth() {
    if (node.clientWidth !== lastWidth) schedule();
  }

  const widthObserver = new ResizeObserver(scheduleForWidth);
  widthObserver.observe(node);
  if (node.parentElement) widthObserver.observe(node.parentElement);
  const textObserver = new MutationObserver(schedule);
  textObserver.observe(node, { characterData: true, childList: true, subtree: true });
  if (typeof document !== "undefined" && document.fonts) void document.fonts.ready.then(schedule);

  // Fit synchronously for the first paint, then once more after the browser has
  // settled layout and fonts.
  fit();
  schedule();

  return {
    update(next: FitOneLineOptions = {}) {
      minScale = next.minScale ?? DEFAULT_MIN_SCALE;
      schedule();
    },
    destroy() {
      if (frame) cancelAnimationFrame(frame);
      widthObserver.disconnect();
      textObserver.disconnect();
      node.style.fontSize = "";
      node.style.whiteSpace = previous.whiteSpace;
      node.style.overflow = previous.overflow;
      node.style.textOverflow = previous.textOverflow;
    }
  };
}
