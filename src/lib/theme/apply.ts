import type { ColorMode, ThemePalette } from "$lib/ipc/contracts";

let systemScheme: MediaQueryList | null = null;
let systemListener: ((event: MediaQueryListEvent) => void) | null = null;

export function paletteAttribute(palette: ThemePalette): string {
  return palette === "dawnWheat" ? "dawn-wheat" : "quiet-daybook";
}

function resolveColorMode(mode: ColorMode): "light" | "dark" {
  if (mode !== "system") return mode;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function stopSystemListener() {
  if (systemScheme && systemListener) systemScheme.removeEventListener("change", systemListener);
  systemScheme = null;
  systemListener = null;
}

/**
 * Applies the persisted palette and color mode without touching layout or app
 * state. System mode stays live by listening to the Windows color-scheme media
 * query in each webview.
 */
export function applyTheme(
  palette: ThemePalette,
  colorMode: ColorMode,
  paintCanvas = true,
) {
  if (typeof document === "undefined" || typeof window === "undefined") return;
  document.documentElement.dataset.themePalette = paletteAttribute(palette);
  document.documentElement.dataset.colorMode = resolveColorMode(colorMode);
  document.documentElement.classList.toggle("canvas", paintCanvas);

  stopSystemListener();
  if (colorMode === "system") {
    systemScheme = window.matchMedia("(prefers-color-scheme: dark)");
    systemListener = (event) => {
      document.documentElement.dataset.colorMode = event.matches ? "dark" : "light";
    };
    systemScheme.addEventListener("change", systemListener);
  }
}
