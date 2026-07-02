import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { FileNode } from "../types";

interface Params {
  instanceId: string;
  rootDir: string;
}

type ChildrenState =
  | { kind: "loading" }
  | { kind: "loaded"; nodes: FileNode[] }
  | { kind: "error"; message: string };

interface TreeState {
  /** path → 子ノード状態。空文字キーはルート直下。 */
  childrenByPath: Record<string, ChildrenState>;
  /** 展開中のディレクトリ path 集合。 */
  expanded: Set<string>;
}

/**
 * ディレクトリツリー本体。展開時に遅延ロード、子ノードはキャッシュ、
 * 展開状態は localStorage にインスタンス単位で永続化する。
 * `rootDir` 変更時は展開・キャッシュをリセットしてルートから読み直す。
 */
export function useExplorerTree({ instanceId, rootDir }: Params) {
  // 展開状態はインスタンス＋ルート単位で永続化。ルート切替で別キーになる。
  const storageKey = `ccc.explorer.expanded.${instanceId}:${rootDir}`;

  const [state, setState] = useState<TreeState>(() => ({
    childrenByPath: {},
    expanded: loadExpanded(storageKey),
  }));

  // インスタンス切替 / ルート変更時に state をリセット（展開状態は localStorage から復元）。
  useEffect(() => {
    setState({ childrenByPath: {}, expanded: loadExpanded(storageKey) });
  }, [instanceId, rootDir, storageKey]);

  // 展開状態は変わるたび永続化。Set を JSON に出すため配列化。
  useEffect(() => {
    try {
      localStorage.setItem(storageKey, JSON.stringify(Array.from(state.expanded)));
    } catch {
      // 保存失敗は無視
    }
  }, [state.expanded, storageKey]);

  const loadChildren = useCallback(
    async (path: string) => {
      setState((prev) => ({
        ...prev,
        childrenByPath: { ...prev.childrenByPath, [path]: { kind: "loading" } },
      }));
      try {
        const nodes = await invoke<FileNode[]>("explorer_list_directory", {
          instanceId,
          root: rootDir,
          path,
        });
        setState((prev) => ({
          ...prev,
          childrenByPath: { ...prev.childrenByPath, [path]: { kind: "loaded", nodes } },
        }));
      } catch (e) {
        setState((prev) => ({
          ...prev,
          childrenByPath: {
            ...prev.childrenByPath,
            [path]: { kind: "error", message: String(e) },
          },
        }));
      }
    },
    [instanceId, rootDir]
  );

  // ルート直下 + localStorage から復元された展開済みディレクトリを起動時に再ロード。
  // 展開フラグだけ復元して子を取らないと、フォルダが「開いた」表示なのに中身が描画されない
  // 不整合状態になるため、復元時はまとめて先読みする。
  useEffect(() => {
    if (!rootDir) return;
    loadChildren("");
    for (const path of loadExpanded(storageKey)) {
      loadChildren(path);
    }
  }, [rootDir, storageKey, loadChildren]);

  const toggle = useCallback(
    async (path: string) => {
      const isExpanded = state.expanded.has(path);
      setState((prev) => {
        const next = new Set(prev.expanded);
        if (isExpanded) {
          next.delete(path);
        } else {
          next.add(path);
        }
        return { ...prev, expanded: next };
      });
      // 未ロードなら同時に子を取得
      if (!isExpanded && !state.childrenByPath[path]) {
        await loadChildren(path);
      }
    },
    [state.expanded, state.childrenByPath, loadChildren]
  );

  const refresh = useCallback(
    async (path: string) => {
      await loadChildren(path);
    },
    [loadChildren]
  );

  const refreshAll = useCallback(async () => {
    // 展開済みディレクトリすべてを再取得
    const paths = ["", ...Array.from(state.expanded)];
    await Promise.all(paths.map((p) => loadChildren(p)));
  }, [state.expanded, loadChildren]);

  return {
    childrenByPath: state.childrenByPath,
    expanded: state.expanded,
    toggle,
    refresh,
    refreshAll,
  };
}

function loadExpanded(key: string): Set<string> {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return new Set();
    const arr = JSON.parse(raw);
    if (Array.isArray(arr)) return new Set(arr.filter((s): s is string => typeof s === "string"));
  } catch {
    // 無視
  }
  return new Set();
}
