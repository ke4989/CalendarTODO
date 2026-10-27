/** A drag-owned animation loop: no idle timer, persistence, or synthetic pointer events. */
export type DragPoint = Pick<PointerEvent, "clientX" | "clientY" | "pointerId">;

export function startReorderAutoScroll(
  scroller: HTMLElement,
  list: HTMLElement,
  initial: DragPoint,
  onScrolled: (point: DragPoint) => void,
) {
  let point: DragPoint = copy(initial);
  let frame: number | null = null;
  let previousTime: number | null = null;
  let stopped = false;
  let remainder = 0;
  const page = scroller === document.scrollingElement;

  function copy(value: DragPoint): DragPoint {
    return { clientX: value.clientX, clientY: value.clientY, pointerId: value.pointerId };
  }

  function velocity(): number {
    if (stopped || !scroller.isConnected || !list.isConnected) return 0;
    const rect = list.getBoundingClientRect();
    const viewport = page ? { top: 0, bottom: window.innerHeight } : scroller.getBoundingClientRect();
    const top = Math.max(0, rect.top, viewport.top);
    const bottom = Math.min(window.innerHeight, rect.bottom, viewport.bottom);
    const left = Math.max(0, rect.left), right = Math.min(window.innerWidth, rect.right);
    if (bottom <= top || point.clientX < left || point.clientX > right || point.clientY < top || point.clientY > bottom) return 0;
    const edge = Math.min(56, (bottom - top) / 3);
    const strength = point.clientY < top + edge ? -(1 - (point.clientY - top) / edge)
      : point.clientY > bottom - edge ? 1 - (bottom - point.clientY) / edge : 0;
    const maxScroll = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
    if ((strength < 0 && scroller.scrollTop <= 0) || (strength > 0 && scroller.scrollTop >= maxScroll)) return 0;
    return strength * 640;
  }

  function pause() {
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
    previousTime = null;
    remainder = 0;
  }

  function step(time: number) {
    frame = null;
    const speed = velocity();
    if (!speed) { previousTime = null; return; }
    const elapsed = previousTime === null ? 1 / 60 : Math.min(32, Math.max(0, time - previousTime)) / 1000;
    previousTime = time;
    const before = scroller.scrollTop;
    const distance = speed * elapsed + remainder;
    const pixels = Math.trunc(distance);
    remainder = distance - pixels;
    scroller.scrollTop = Math.max(0, Math.min(scroller.scrollHeight - scroller.clientHeight, before + pixels));
    if (scroller.scrollTop !== before) onScrolled(point);
    if (velocity()) frame = requestAnimationFrame(step);
    else previousTime = null;
  }

  function update(value: DragPoint) {
    if (stopped || value.pointerId !== point.pointerId) return;
    point = copy(value);
    if (!velocity()) pause();
    else if (frame === null) frame = requestAnimationFrame(step);
  }

  update(initial);
  return { update, stop() { stopped = true; pause(); } };
}
