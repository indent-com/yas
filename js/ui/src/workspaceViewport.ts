export interface WorkspaceViewport {
  height: number;
  offsetTop: number;
  baselineHeight: number;
}

/** Track keyboard occlusion against the shell's CSS viewport, not Safari's
 * larger layout viewport, which can include the browser's own chrome. */
export function observeWorkspaceViewport(
  onChange: (viewport: WorkspaceViewport) => void,
): () => void {
  const viewport = window.visualViewport;
  if (!viewport) return () => {};

  let baseWidth = 0;
  let baselineHeight = 0;
  let orientationTimer: ReturnType<typeof setTimeout> | undefined;
  const update = () => {
    const { height, width, offsetTop } = viewport;
    // The app shell sizes html to 100dvh. innerHeight (and the root's
    // clientHeight) can describe a different viewport on mobile Safari.
    const cssHeight = document.documentElement.getBoundingClientRect().height;
    const fullHeight = Math.max(height, cssHeight || window.innerHeight);
    if (
      baseWidth === 0 ||
      Math.abs(width - baseWidth) > 48 ||
      fullHeight > baselineHeight ||
      baselineHeight - height <= 150
    ) {
      // Reset on rotation, grow with collapsed browser bars, and follow small
      // chrome changes. Keep the baseline when a keyboard resizes the shell.
      baseWidth = width;
      baselineHeight = fullHeight;
    }
    onChange({ height, offsetTop, baselineHeight });
  };
  const onOrientationChange = () => {
    clearTimeout(orientationTimer);
    orientationTimer = setTimeout(update, 150);
  };

  update();
  viewport.addEventListener("resize", update);
  viewport.addEventListener("scroll", update);
  window.addEventListener("resize", update);
  screen.orientation?.addEventListener("change", onOrientationChange);
  return () => {
    clearTimeout(orientationTimer);
    viewport.removeEventListener("resize", update);
    viewport.removeEventListener("scroll", update);
    window.removeEventListener("resize", update);
    screen.orientation?.removeEventListener("change", onOrientationChange);
  };
}
