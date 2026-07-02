import { useEffect } from "react";
import type { FileNode } from "../../types";

interface Props {
  x: number;
  y: number;
  node: FileNode;
  onDownload: (node: FileNode) => void;
  onClose: () => void;
}

/**
 * Explorer ツリーノード右クリック時の浮動メニュー。
 * 現状はリモートインスタンス向けに「ダウンロード」項目のみを提供する。
 * 親側で `isRemote` の判定をしてからマウントすること。
 */
export function ExplorerContextMenu({ x, y, node, onDownload, onClose }: Props) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // 画面右端・下端からはみ出さないように軽くクランプする（メニュー幅は ~220px 想定）。
  const left = Math.min(x, window.innerWidth - 240);
  const top = Math.min(y, window.innerHeight - 80);

  return (
    <>
      <div
        className="explorer-context-overlay"
        onClick={onClose}
        onContextMenu={(e) => {
          e.preventDefault();
          onClose();
        }}
      />
      <div className="explorer-context-menu" style={{ left, top }} role="menu">
        <button
          className="explorer-context-item"
          onClick={() => onDownload(node)}
          role="menuitem"
        >
          <span className="explorer-context-icon">⤓</span>
          <span className="explorer-context-label">
            ダウンロード <span className="explorer-context-hint">~/Downloads/</span>
          </span>
        </button>
      </div>
    </>
  );
}
