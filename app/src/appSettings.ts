import type { AppSettings } from "./types";
import { setNotificationsEnabled } from "./notify";

/**
 * Apply settings that live in the page rather than the engine.
 *
 * The accent is one variable by design: the theme derives its hover and soft variants
 * from it here, and picks dark or light text to ride on top so any colour stays legible.
 */
export function applySettings(settings: AppSettings): void {
  setNotificationsEnabled(settings.notifications);

  const root = document.documentElement.style;
  const accent = settings.accent;
  if (!accent) {
    for (const name of ["--accent", "--accent-strong", "--accent-soft", "--on-accent"]) {
      root.removeProperty(name);
    }
    return;
  }
  root.setProperty("--accent", accent);
  root.setProperty("--accent-strong", `color-mix(in srgb, ${accent} 82%, white)`);
  root.setProperty("--accent-soft", `color-mix(in srgb, ${accent} 16%, transparent)`);
  root.setProperty("--on-accent", isLight(accent) ? "#1d1418" : "#fdf6f9");
}

/** Relative luminance above the midpoint — enough to choose between two text colours. */
function isLight(hex: string): boolean {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  const linear = (c: number) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b) > 0.4;
}
