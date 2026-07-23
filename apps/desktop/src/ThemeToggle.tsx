import { useCallback, useEffect, useState } from "react";
import { Icon } from "./Icons";

type Theme = "light" | "dark";

const STORAGE_KEY = "lumen-theme";

function currentTheme(): Theme {
  return document.documentElement.getAttribute("data-theme") === "dark"
    ? "dark"
    : "light";
}

/**
 * Atelier ⇄ Vault theme toggle. Flips `data-theme` on <html> and persists
 * to localStorage. The no-flash boot script in index.html sets the initial
 * value before first paint; this only mutates it afterwards. Shows the moon
 * in light mode (tap → Vault) and the sun in dark mode (tap → Atelier).
 */
export function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>(() => currentTheme());

  // Reflect an external change (e.g. another window) if the attribute moves.
  useEffect(() => {
    setTheme(currentTheme());
  }, []);

  const toggle = useCallback(() => {
    const next: Theme = theme === "dark" ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", next);
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      /* storage disabled — theme still applies for this session */
    }
    setTheme(next);
  }, [theme]);

  const toVault = theme !== "dark";
  return (
    <button
      type="button"
      className="icon-btn"
      onClick={toggle}
      title={toVault ? "切换到 Vault（暗色）" : "切换到 Atelier（暖光）"}
      aria-label={toVault ? "切换到暗色主题" : "切换到暖光主题"}
    >
      <Icon name={toVault ? "moon" : "sun"} size={16} />
    </button>
  );
}
