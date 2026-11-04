import { create } from "zustand";

export type Theme = "dark" | "light";

const STORAGE_KEY = "octra.theme";

function readInitialTheme(): Theme {
  if (typeof window === "undefined") {
    return "dark";
  }
  const attr = document.documentElement.dataset.theme;
  if (attr === "light" || attr === "dark") {
    return attr;
  }
  const stored = window.localStorage.getItem(STORAGE_KEY);
  return stored === "light" ? "light" : "dark";
}

function applyTheme(theme: Theme) {
  if (typeof document !== "undefined") {
    document.documentElement.dataset.theme = theme;
  }
  if (typeof window !== "undefined") {
    window.localStorage.setItem(STORAGE_KEY, theme);
  }
}

interface ThemeState {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  toggleTheme: () => void;
}

export const useTheme = create<ThemeState>((set, get) => ({
  theme: readInitialTheme(),
  setTheme: (theme) => {
    applyTheme(theme);
    set({ theme });
  },
  toggleTheme: () => {
    const next = get().theme === "dark" ? "light" : "dark";
    applyTheme(next);
    set({ theme: next });
  },
}));
