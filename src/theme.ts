/** Tema de la interfaz: oscuro, claro o el del sistema. Solo la barra lateral
 * y sus diálogos; las apps siguen con el suyo. Se aplica al importar el
 * módulo, antes de pintar nada. */

export type Theme = "dark" | "light" | "system";

export const THEMES: Theme[] = ["dark", "light", "system"];

const STORAGE_KEY = "pulpo.theme";
const systemLight = window.matchMedia("(prefers-color-scheme: light)");

function loadTheme(): Theme {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved && (THEMES as string[]).includes(saved)) return saved as Theme;
  } catch {
    /* sin almacenamiento: el de por defecto */
  }
  return "dark";
}

let theme: Theme = loadTheme();

export const currentTheme = () => theme;

/** Variables CSS en `:root[data-theme]` (ver `styles.css`). */
function applyTheme() {
  const resolved = theme === "system" ? (systemLight.matches ? "light" : "dark") : theme;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

export function setTheme(next: Theme) {
  theme = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    /* no se recuerda */
  }
  applyTheme();
}

systemLight.addEventListener("change", () => theme === "system" && applyTheme());
applyTheme();
