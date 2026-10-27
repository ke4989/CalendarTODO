// Executed only by Rust for the desktop window. The base document remains
// opaque; the animation owns its temporary opacity and always expires.
// Tokens are (generation, episode). A cancel for a token also tombstones it, so
// a preparation script that runs late can never start a fade after fallback.
((generation, episode, cancel) => {
  try {
    const key = "__calendarTodoDesktopReveal";
    const previous = window[key];
    if (previous) {
      const newer = previous.generation > generation ||
        (previous.generation === generation && previous.episode > episode);
      const same = previous.generation === generation && previous.episode === episode;
      if (newer || (same && !cancel)) return "stale";
    }
    previous?.animation?.cancel();
    const entry = { generation, episode, animation: null };
    window[key] = entry;
    if (cancel) return "cancelled";
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return "prepared";
    entry.animation = document.documentElement.animate(
      [{ opacity: 0 }, { opacity: 1 }],
      { duration: 1000, delay: 250, easing: "cubic-bezier(.4,0,.2,1)", fill: "backwards" }
    );
    entry.animation.finished.then(() => {
      if (window[key] === entry) entry.animation = null;
    }, () => {});
    return "prepared";
  } catch {
    return "error";
  }
})(__GENERATION__, __EPISODE__, __CANCEL__);
