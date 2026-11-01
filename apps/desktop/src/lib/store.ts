import { create } from "zustand";
import { persist } from "zustand/middleware";

interface ControlSettingsState {
  baseUrl: string;
  token: string;
  setBaseUrl: (baseUrl: string) => void;
  setToken: (token: string) => void;
}

export const useControlSettings = create<ControlSettingsState>()(
  persist(
    (set) => ({
      baseUrl: "http://127.0.0.1:8081",
      token: "",
      setBaseUrl: (baseUrl) => set({ baseUrl }),
      setToken: (token) => set({ token }),
    }),
    { name: "octra.control-settings" },
  ),
);
