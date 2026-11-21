import { flushSync } from "react-dom";

let stopZoom: (() => void) | undefined;

export function stopTreemapTransition() {
  stopZoom?.();
}

export async function transitionTreemap(
  stage: HTMLElement | null,
  path: string,
  direction: "in" | "out",
  update: () => void,
) {
  stopTreemapTransition();
  const canvas = stage?.querySelector<HTMLElement>(".treemap-canvas");
  const findTile = (map: HTMLElement) =>
    map.querySelector<HTMLElement>(`[data-directory-tile="${CSS.escape(path)}"]`);
  const source = canvas && (direction === "in" ? findTile(canvas) : canvas);

  if (!stage || !canvas || !source?.offsetWidth || !source.offsetHeight ||
    !document.startViewTransition ||
    !CSS.supports("selector(:active-view-transition-type(treemap-in))") ||
    matchMedia("(prefers-reduced-motion: reduce)").matches) {
    update();
    return;
  }

  const active = document.activeElement;
  const restoreFocus = canvas.contains(active) && !!active?.matches(":focus-visible");
  const named = new Set<HTMLElement>();
  const name = (element: HTMLElement, value: string) => {
    element.style.viewTransitionName = value;
    named.add(element);
  };
  const clearNames = () => {
    for (const element of named) element.style.removeProperty("view-transition-name");
    named.clear();
  };
  if (direction === "in") name(canvas, "treemap-surround");
  name(source, "treemap-portal");

  let focusTarget: HTMLElement | null | undefined;
  const transition = document.startViewTransition({
    types: [`treemap-${direction}`],
    update: () => {
      clearNames();
      flushSync(update);
      const next = stage.querySelector<HTMLElement>(".treemap-canvas");
      const target = next && (direction === "out" ? findTile(next) : next);
      if (!next || !target?.offsetWidth || !target.offsetHeight) {
        transition.skipTransition();
        return;
      }
      if (direction === "out") name(next, "treemap-surround");
      name(target, "treemap-portal");
      focusTarget = target.querySelector<HTMLElement>(direction === "out"
        ? ".treemap-hit"
        : '.treemap-hit[tabindex="0"]');
    },
  });

  const stop = () => {
    transition.skipTransition();
    clearNames();
  };
  stopZoom = stop;
  window.addEventListener("resize", stop, { once: true });
  void transition.ready.catch(() => {}); // An interrupted or hidden transition can skip its snapshots.
  const finish = () => {
    window.removeEventListener("resize", stop);
    clearNames();
    if (stopZoom !== stop) return;
    stopZoom = undefined;
    if (restoreFocus && document.activeElement === document.body) {
      focusTarget?.focus({ preventScroll: true });
    }
  };
  void transition.finished.then(finish, finish);
  await transition.updateCallbackDone;
}
