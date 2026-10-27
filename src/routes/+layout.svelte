<script lang="ts">
  import { onMount } from "svelte";
  import type { Snippet } from "svelte";
  import TextEditMenu from "$lib/components/TextEditMenu.svelte";
  import "$lib/styles/tokens.css";
  import "$lib/styles/base.css";

  let { children }: { children: Snippet } = $props();

  onMount(() => {
    const timers = new Map<HTMLElement, ReturnType<typeof setTimeout>>();
    let hovered: HTMLElement | null = null;
    const scrollElement = (event: Event) => event.target instanceof HTMLElement
      ? event.target
      : document.scrollingElement instanceof HTMLElement ? document.scrollingElement : null;
    const showScrollbar = (event: Event) => {
      const element = scrollElement(event);
      if (!element) return;
      element.classList.add("scrollbar-active");
      const previous = timers.get(element);
      if (previous) clearTimeout(previous);
      timers.set(element, setTimeout(() => {
        element.classList.remove("scrollbar-active");
        timers.delete(element);
      }, 1_200));
    };
    const moveNearScrollbar = (event: PointerEvent) => {
      const next = event.composedPath().find((node) => {
        if (!(node instanceof HTMLElement)) return false;
        const style = getComputedStyle(node);
        const vertical = node.scrollHeight > node.clientHeight && /auto|scroll|overlay/.test(style.overflowY);
        const horizontal = node.scrollWidth > node.clientWidth && /auto|scroll|overlay/.test(style.overflowX);
        if (!vertical && !horizontal) return false;
        const rect = node.getBoundingClientRect();
        return vertical && event.clientX >= rect.right - 12 || horizontal && event.clientY >= rect.bottom - 12;
      }) as HTMLElement | undefined ?? null;
      if (hovered === next) return;
      hovered?.classList.remove("scrollbar-hover");
      hovered = next;
      hovered?.classList.add("scrollbar-hover");
    };
    const clearHover = () => { hovered?.classList.remove("scrollbar-hover"); hovered = null; };
    document.addEventListener("scroll", showScrollbar, true);
    document.addEventListener("pointermove", moveNearScrollbar, true);
    document.addEventListener("pointerleave", clearHover);
    window.addEventListener("blur", clearHover);
    return () => {
      document.removeEventListener("scroll", showScrollbar, true);
      document.removeEventListener("pointermove", moveNearScrollbar, true);
      document.removeEventListener("pointerleave", clearHover);
      window.removeEventListener("blur", clearHover);
      clearHover();
      timers.forEach((timer) => clearTimeout(timer));
    };
  });
</script>

{@render children()}
<TextEditMenu />
