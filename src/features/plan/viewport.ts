// Where the diagram starts: a readable zoom beats seeing the whole plan.

export interface Size {
  width: number;
  height: number;
}

export interface ViewportState {
  x: number;
  y: number;
  zoom: number;
}

/** Below this fit zoom the cards are too small to read. */
export const MIN_READABLE_ZOOM = 0.65;
/** Zoom used instead when the whole plan would not be readable. */
export const READABLE_ZOOM = 0.85;
const EDGE_PADDING = 24;
const FIT_PADDING = 0.15;
const MIN_ZOOM = 0.1;
const MAX_ZOOM = 2;

/** The viewport that shows all of `bounds` with the usual padding (what "Fit to view" does). */
export function fitViewport(bounds: Size, view: Size): ViewportState {
  const zoom = Math.max(
    MIN_ZOOM,
    Math.min(
      MAX_ZOOM,
      view.width / (bounds.width * (1 + 2 * FIT_PADDING)),
      view.height / (bounds.height * (1 + 2 * FIT_PADDING)),
    ),
  );
  return {
    zoom,
    x: (view.width - bounds.width * zoom) / 2,
    y: (view.height - bounds.height * zoom) / 2,
  };
}

/**
 * First viewport of a diagram: fit everything when that stays readable, otherwise a readable
 * zoom with the root operator in the top-left corner.
 */
export function initialViewport(
  bounds: Size,
  view: Size,
  root: { x: number; y: number },
): ViewportState {
  const fit = fitViewport(bounds, view);
  if (fit.zoom >= MIN_READABLE_ZOOM) return fit;
  return {
    zoom: READABLE_ZOOM,
    x: EDGE_PADDING - root.x * READABLE_ZOOM,
    y: EDGE_PADDING - root.y * READABLE_ZOOM,
  };
}
