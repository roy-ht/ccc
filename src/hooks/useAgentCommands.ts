import { useState, useCallback } from "react";

const STORAGE_KEY = "ccc:agent-commands";
const DEFAULT_COMMANDS = ["claude"];

function load(): string[] {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return DEFAULT_COMMANDS;
    const parsed = JSON.parse(stored);
    return Array.isArray(parsed) && parsed.length > 0 ? parsed : DEFAULT_COMMANDS;
  } catch {
    return DEFAULT_COMMANDS;
  }
}

export function useAgentCommands() {
  const [commands, setCommands] = useState<string[]>(load);

  const addCommand = useCallback((cmd: string) => {
    const trimmed = cmd.trim();
    if (!trimmed) return;
    setCommands((prev) => {
      if (prev.includes(trimmed)) return prev;
      const next = [...prev, trimmed];
      localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
      return next;
    });
  }, []);

  const removeCommand = useCallback((cmd: string) => {
    setCommands((prev) => {
      const next = prev.filter((c) => c !== cmd);
      if (next.length === 0) return prev;
      localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
      return next;
    });
  }, []);

  return { commands, addCommand, removeCommand };
}
