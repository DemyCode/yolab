import { useCallback, useEffect, useState } from "react";

export type ThemeChoice = "light" | "dark" | "system";

const KEY = "yolab.theme";

function systemPrefersDark(): boolean {
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
}

function resolve(choice: ThemeChoice): "light" | "dark" {
  return choice === "system"
    ? systemPrefersDark()
      ? "dark"
      : "light"
    : choice;
}

function apply(choice: ThemeChoice) {
  document.documentElement.classList.toggle("dark", resolve(choice) === "dark");
}

export function readThemeChoice(): ThemeChoice {
  const stored = localStorage.getItem(KEY);
  return stored === "light" || stored === "dark" ? stored : "system";
}

/**
 * Applied before React mounts (see main.tsx) so the first painted frame is
 * already the right colour. Doing it in an effect produces a white flash on
 * every load for dark-mode users, which looks broken.
 */
export function initTheme() {
  apply(readThemeChoice());
}

export function useTheme() {
  const [choice, setChoice] = useState<ThemeChoice>(readThemeChoice);

  const set = useCallback((next: ThemeChoice) => {
    setChoice(next);
    if (next === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, next);
    apply(next);
  }, []);

  // Follow the OS live while the choice is "system" — someone with a sunset
  // schedule expects the app to turn with everything else.
  useEffect(() => {
    if (choice !== "system") return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => apply("system");
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [choice]);

  return { choice, resolved: resolve(choice), setTheme: set };
}
