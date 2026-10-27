export interface DialogFocusOptions {
  close: () => void;
  disabled?: boolean;
}

export function createBackdropClickGuard() {
  let startedOnBackdrop = false;
  let endedOnBackdrop = false;
  const reset = () => { startedOnBackdrop = false; endedOnBackdrop = false; };
  return {
    pointerDown(target: EventTarget | null, backdrop: EventTarget | null) {
      startedOnBackdrop = target === backdrop;
      endedOnBackdrop = false;
    },
    pointerUp(target: EventTarget | null, backdrop: EventTarget | null) {
      endedOnBackdrop = target === backdrop;
    },
    pointerCancel: reset,
    click(target: EventTarget | null, backdrop: EventTarget | null) {
      const dismiss = startedOnBackdrop && endedOnBackdrop && target === backdrop;
      reset();
      return dismiss;
    }
  };
}

const selector = [
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "[href]",
  '[tabindex]:not([tabindex="-1"])'
].join(",");

export function dialogFocus(node: HTMLElement, initial: DialogFocusOptions) {
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const inerted: { element: HTMLElement; wasInert: boolean }[] = [];
  let options = initial;

  let branch: HTMLElement | null = node.closest<HTMLElement>(".modal-scrim, .scrim") ?? node;
  while (branch.parentElement && branch.parentElement !== document.body) {
    for (const sibling of branch.parentElement.children) {
      if (sibling !== branch && sibling instanceof HTMLElement) {
        inerted.push({ element: sibling, wasInert: sibling.inert });
        sibling.inert = true;
      }
    }
    branch = branch.parentElement;
  }

  const focusables = () => [...node.querySelectorAll<HTMLElement>(selector)].filter((item) => item.offsetParent !== null);
  const focusInitial = () => {
    const target = node.querySelector<HTMLElement>("[data-autofocus]") ?? focusables()[0] ?? node;
    target.focus();
  };
  const onKeydown = (event: KeyboardEvent) => {
    if (event.key === "Escape" && !options.disabled) {
      event.preventDefault();
      event.stopPropagation();
      options.close();
      return;
    }
    if (event.key !== "Tab") return;
    const items = focusables();
    if (!items.length) {
      event.preventDefault();
      node.focus();
      return;
    }
    const first = items[0], last = items[items.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  node.addEventListener("keydown", onKeydown);
  queueMicrotask(focusInitial);
  return {
    update(next: DialogFocusOptions) { options = next; },
    destroy() {
      node.removeEventListener("keydown", onKeydown);
      for (const { element, wasInert } of inerted) element.inert = wasInert;
      queueMicrotask(() => { if (previous?.isConnected) previous.focus(); });
    }
  };
}
