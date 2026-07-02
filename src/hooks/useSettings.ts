import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings } from "../types";
import { DEFAULT_TERMINAL_THEME_ID } from "../constants";

const DEFAULT_SETTINGS: AppSettings = {
  display: {
    font_family: '"Cascadia Code", "Fira Code", "JetBrains Mono", monospace',
    font_size: 14,
    color_theme: DEFAULT_TERMINAL_THEME_ID,
    scrollback_lines: 1000,
    status_message_lines: 2,
  },
  connections: [],
};

export function useSettings() {
  const [settings, setSettings] = useState<AppSettings>(DEFAULT_SETTINGS);

  useEffect(() => {
    invoke<AppSettings>("load_settings")
      .then(setSettings)
      .catch(() => {});
  }, []);

  const saveSettings = useCallback(async (updated: AppSettings) => {
    setSettings(updated);
    await invoke("save_settings", { settings: updated });
  }, []);

  return { settings, saveSettings };
}
