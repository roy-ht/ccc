import { useCallback, useEffect, useRef, useState } from "react";

const STORAGE_KEY = "ccc.explorer-split-width";
export const EXPLORER_SPLIT_DEFAULT = 320;
export const EXPLORER_SPLIT_MIN = 200;
export const EXPLORER_SPLIT_MAX = 800;

function clamp(n: number): number {
  return Math.min(EXPLORER_SPLIT_MAX, Math.max(EXPLORER_SPLIT_MIN, n));
}

function loadInitial(): number {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const n = Number.parseInt(raw, 10);
      if (Number.isFinite(n)) return clamp(n);
    }
  } catch {
    // localStorage 不可なら無視
  }
  return EXPLORER_SPLIT_DEFAULT;
}

/**
 * Explorer 左ペイン（ツリー）の幅をドラッグで調整可能にし、localStorage に永続化する。
 * `useSidebarWidth` と同じ実装パターン。
 *
 * `containerLeft` には Splitter を含むコンテナの左端 X 座標（getBoundingClientRect の left）
 * を渡す。マウス座標からこの値を引いた相対 X を左ペイン幅とする。
 */
export function useExplorerSplit() {
  const [width, setWidth] = useState<number>(loadInitial);
  const draggingRef = useRef(false);
  const containerLeftRef = useRef(0);

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, String(width));
    } catch {
      // 保存失敗は無視
    }
  }, [width]);

  const startDrag = useCallback((e: React.MouseEvent, containerLeft: number) => {
    e.preventDefault();
    if (draggingRef.current) return;
    draggingRef.current = true;
    containerLeftRef.current = containerLeft;
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";

    const onMove = (ev: MouseEvent) => {
      if (!draggingRef.current) return;
      setWidth(clamp(ev.clientX - containerLeftRef.current));
    };
    const onUp = () => {
      draggingRef.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }, []);

  const resetWidth = useCallback(() => {
    setWidth(EXPLORER_SPLIT_DEFAULT);
  }, []);

  return { width, startDrag, resetWidth };
}
