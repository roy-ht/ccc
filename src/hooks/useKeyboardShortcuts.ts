import { useEffect } from "react";
import { InstanceId, InstanceInfo } from "../types";

interface Options {
  instances: InstanceInfo[];
  activeId: InstanceId | null;
  onNew: () => void;
  onSelectById: (id: InstanceId) => void;
}

/**
 * グローバルキーボードショートカットを登録する。
 * xterm.js がフォーカスを持っていても確実に拾えるよう capture フェーズで処理する。
 */
export function useKeyboardShortcuts({
  instances,
  activeId,
  onNew,
  onSelectById,
}: Options) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey; // macOS Cmd
      if (!meta) return;

      // Cmd+T → 新規インスタンスダイアログ
      if (e.key === "t") {
        e.preventDefault();
        onNew();
        return;
      }

      // 「閉じる」ショートカット（Cmd+W）は誤爆が多いため廃止。
      // インスタンスを閉じる操作はサイドバーの ✕ ボタン（確認ダイアログ付き）から行う。

      // Cmd+[ → 前のインスタンスへ
      if (e.key === "[") {
        e.preventDefault();
        if (!activeId || instances.length === 0) return;
        const idx = instances.findIndex((s) => s.id === activeId);
        const prev = instances[(idx - 1 + instances.length) % instances.length];
        onSelectById(prev.id);
        return;
      }

      // Cmd+] → 次のインスタンスへ
      if (e.key === "]") {
        e.preventDefault();
        if (!activeId || instances.length === 0) return;
        const idx = instances.findIndex((s) => s.id === activeId);
        const next = instances[(idx + 1) % instances.length];
        onSelectById(next.id);
        return;
      }

      // Cmd+1〜9 → n番目のインスタンスへ
      const num = parseInt(e.key, 10);
      if (num >= 1 && num <= 9) {
        e.preventDefault();
        const target = instances[num - 1];
        if (target) onSelectById(target.id);
        return;
      }
    };

    // capture=true でxterm.jsより先にイベントを受け取る
    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [instances, activeId, onNew, onSelectById]);
}
