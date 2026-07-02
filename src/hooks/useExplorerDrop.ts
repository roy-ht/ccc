import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { invoke } from "@tauri-apps/api/core";
import type { CopySummary } from "../types";

interface Params {
  instanceId: string;
  /** コピー先のルート（Explorer の現在 cwd）。 */
  rootDir: string;
  /** Explorer タブがアクティブか。false のときはドロップを処理しない。 */
  active: boolean;
  /** コピー完了後にツリーをリロードするためのコールバック。 */
  onCompleted?: (summary: CopySummary) => void;
}

interface DropState {
  /** ドラッグオーバー中（オーバーレイ表示用）。 */
  hovering: boolean;
  /** コピー実行中（ブロッキングオーバーレイ表示用）。 */
  copying: boolean;
  /** 直近の結果（フッターの結果トースト用）。 */
  lastResult: CopySummary | null;
  /** 直近のエラー（invoke 自体が失敗した場合）。 */
  lastError: string | null;
}

/**
 * Tauri webview の Drag & Drop イベントを購読し、Explorer のカレントディレクトリへ
 * ローカルファイル/ディレクトリをコピーする。Explorer タブがアクティブな時のみ動作する。
 *
 * コピー先 (`rootDir`) は Explorer の現在 cwd 直下（dest_rel="" で渡す）。
 * ツリーノードへの個別ドロップは v0.5 範囲外（cwd 直下にまとめる）。
 */
export function useExplorerDrop({ instanceId, rootDir, active, onCompleted }: Params) {
  const [state, setState] = useState<DropState>({
    hovering: false,
    copying: false,
    lastResult: null,
    lastError: null,
  });

  // ref で最新値を listener から参照する（listener は 1 回しか登録しないため）
  const instanceIdRef = useRef(instanceId);
  const rootDirRef = useRef(rootDir);
  const activeRef = useRef(active);
  const onCompletedRef = useRef(onCompleted);
  useEffect(() => {
    instanceIdRef.current = instanceId;
  }, [instanceId]);
  useEffect(() => {
    rootDirRef.current = rootDir;
  }, [rootDir]);
  useEffect(() => {
    activeRef.current = active;
  }, [active]);
  useEffect(() => {
    onCompletedRef.current = onCompleted;
  }, [onCompleted]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (!activeRef.current) return;
        const type = event.payload.type;
        if (type === "enter" || type === "over") {
          setState((s) => (s.hovering ? s : { ...s, hovering: true }));
          return;
        }
        if (type === "leave") {
          setState((s) => ({ ...s, hovering: false }));
          return;
        }
        if (type !== "drop") return;
        const paths = event.payload.paths;
        setState((s) => ({ ...s, hovering: false }));
        if (!paths || paths.length === 0) return;
        runCopy(paths);
      })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      });

    const runCopy = async (sources: string[]) => {
      setState((s) => ({ ...s, copying: true, lastError: null }));
      try {
        const summary = await invoke<CopySummary>("explorer_copy_into", {
          instanceId: instanceIdRef.current,
          root: rootDirRef.current,
          destRel: "",
          sources,
        });
        setState((s) => ({ ...s, copying: false, lastResult: summary }));
        onCompletedRef.current?.(summary);
      } catch (e) {
        setState((s) => ({ ...s, copying: false, lastError: String(e) }));
      }
    };

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const dismissResult = useCallback(() => {
    setState((s) => ({ ...s, lastResult: null, lastError: null }));
  }, []);

  return {
    hovering: state.hovering && active,
    copying: state.copying,
    lastResult: state.lastResult,
    lastError: state.lastError,
    dismissResult,
  };
}
