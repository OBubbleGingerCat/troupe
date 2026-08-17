export const VISUAL_VIEWPORTS = {
  desktop: { width: 1280, height: 800 },
  mobile: { width: 390, height: 844 },
} as const;

export type VisualViewportName = keyof typeof VISUAL_VIEWPORTS;
export type VisualScenario = "active" | "archive";
