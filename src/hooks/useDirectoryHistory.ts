import { useCallback } from "react";

const MAX_ENTRIES = 10;

function storageKey(target: string): string {
  return `ccc:dir-history:${target}`;
}

function readHistory(target: string): string[] {
  try {
    const stored = localStorage.getItem(storageKey(target));
    if (!stored) return [];
    const parsed = JSON.parse(stored);
    return Array.isArray(parsed) ? parsed.slice(0, MAX_ENTRIES) : [];
  } catch {
    return [];
  }
}

export function useDirectoryHistory() {
  const getHistory = useCallback((target: string): string[] => {
    return readHistory(target);
  }, []);

  const addToHistory = useCallback((target: string, dir: string) => {
    if (!dir.trim()) return;
    const current = readHistory(target);
    const filtered = current.filter((d) => d !== dir);
    const updated = [dir, ...filtered].slice(0, MAX_ENTRIES);
    localStorage.setItem(storageKey(target), JSON.stringify(updated));
  }, []);

  return { getHistory, addToHistory };
}
