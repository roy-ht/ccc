import { useEffect, useRef } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { InstanceId, InstanceKind, MainTab } from "../types";

interface Params {
  activeId: InstanceId | null;
  activeKind: InstanceKind | null;
  activeTab: MainTab;
  writeToInstance: (id: InstanceId, data: Uint8Array) => Promise<void>;
}

// POSIX シェル向けに single-quote で囲み、内部の ' を '\'' にエスケープする。
// macOS Terminal.app がドラッグ&ドロップしたパスを挿入するときと同じ形式。
function shellQuote(path: string): string {
  return `'${path.replace(/'/g, "'\\''")}'`;
}

/**
 * Tauri webview のファイルドロップを購読し、ドロップされたパスを
 * アクティブなローカルインスタンスの PTY にテキストとして書き込む。
 * リモートインスタンスでは、ローカルパスを送ってもリモート側 Claude Code から
 * 参照できないので無視する。
 * Explorer タブ表示中は Explorer 側の D&D（コピー）が処理するため、
 * ここでは何もしない（activeTab を見て分岐）。
 */
export function useFileDrop({ activeId, activeKind, activeTab, writeToInstance }: Params) {
  const activeIdRef = useRef(activeId);
  const activeKindRef = useRef(activeKind);
  const activeTabRef = useRef(activeTab);
  const writeRef = useRef(writeToInstance);
  useEffect(() => { activeIdRef.current = activeId; }, [activeId]);
  useEffect(() => { activeKindRef.current = activeKind; }, [activeKind]);
  useEffect(() => { activeTabRef.current = activeTab; }, [activeTab]);
  useEffect(() => { writeRef.current = writeToInstance; }, [writeToInstance]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    const encoder = new TextEncoder();

    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type !== "drop") return;
        // Explorer タブ表示中は Explorer 側で処理するので、ターミナルには流さない
        if (activeTabRef.current !== "terminal") return;
        const id = activeIdRef.current;
        if (!id || activeKindRef.current !== "local") return;
        const paths = event.payload.paths;
        if (paths.length === 0) return;
        const text = paths.map(shellQuote).join(" ") + " ";
        writeRef.current(id, encoder.encode(text)).catch(() => {});
      })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
}
