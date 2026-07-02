import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { FileNode, InstanceInfo } from "../../types";
import { useExplorerSplit } from "../../hooks/useExplorerSplit";
import { useExplorerTree } from "../../hooks/useExplorerTree";
import { useExplorerSearch } from "../../hooks/useExplorerSearch";
import { useExplorerDrop } from "../../hooks/useExplorerDrop";
import { ExplorerTree } from "./ExplorerTree";
import { ExplorerSplitter } from "./ExplorerSplitter";
import { ExplorerSearchBar } from "./ExplorerSearchBar";
import { ExplorerSearchResults } from "./ExplorerSearchResults";
import { ExplorerToolbar } from "./ExplorerToolbar";
import { ExplorerContextMenu } from "./ExplorerContextMenu";
import { PreviewPane } from "./preview/PreviewPane";

interface ContextMenuState {
  x: number;
  y: number;
  node: FileNode;
}

interface DownloadState {
  busy: boolean;
  /** 完了時の保存先絶対パス。 */
  savedPath: string | null;
  error: string | null;
}

interface Props {
  instance: InstanceInfo;
}

/**
 * Explorer タブの容器。検索行 / 左ペイン (ツリー or 検索結果) / 右ペイン (プレビュー) の 3 領域構成。
 * `cwd` 状態によってルートを動的に変更できる（ホーム = `instance.directory`）。
 * インスタンス切替時は親 `App.tsx` で `key={instance.id}` を切り替えてマウントし直すので、
 * 状態のリセットはここでは不要。
 */
export function ExplorerPanel({ instance }: Props) {
  const homeDir = instance.directory ?? "";

  // 現在の Explorer ルート。初期値はインスタンス起動ディレクトリ。
  // インスタンス切替で再マウントされるので localStorage 永続化は不要。
  const [cwd, setCwd] = useState<string>(homeDir);

  const split = useExplorerSplit();
  const tree = useExplorerTree({ instanceId: instance.id, rootDir: cwd });
  const search = useExplorerSearch({ instanceId: instance.id, rootDir: cwd });
  const drop = useExplorerDrop({
    instanceId: instance.id,
    rootDir: cwd,
    active: true, // このコンポーネントがマウントされている = Explorer タブがアクティブ
    onCompleted: () => {
      // コピー完了後にツリーを再読み込み
      tree.refreshAll();
    },
  });

  // 選択中ファイル + 行ハイライト
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [highlightLine, setHighlightLine] = useState<number | null>(null);

  // 右クリックメニュー / ダウンロード状態
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [download, setDownload] = useState<DownloadState>({
    busy: false,
    savedPath: null,
    error: null,
  });

  const isRemote = instance.kind === "remote";

  const handleContextMenu = useCallback(
    (e: React.MouseEvent, node: FileNode) => {
      // リモート時のみ右クリックメニューを開く（ローカルはブラウザ既定のままにする）。
      if (!isRemote) return;
      e.preventDefault();
      setContextMenu({ x: e.clientX, y: e.clientY, node });
    },
    [isRemote]
  );

  const handleDownload = useCallback(
    async (node: FileNode) => {
      setContextMenu(null);
      setDownload({ busy: true, savedPath: null, error: null });
      try {
        const result = await invoke<{ saved_path: string }>("explorer_download", {
          instanceId: instance.id,
          root: cwd,
          path: node.path,
        });
        setDownload({ busy: false, savedPath: result.saved_path, error: null });
      } catch (e) {
        setDownload({ busy: false, savedPath: null, error: String(e) });
      }
    },
    [instance.id, cwd]
  );

  const dismissDownloadToast = useCallback(() => {
    setDownload({ busy: false, savedPath: null, error: null });
  }, []);

  const handleSelectFile = useCallback((path: string) => {
    setSelectedPath(path);
    setHighlightLine(null);
  }, []);

  const handleSearchPick = useCallback((path: string, line: number) => {
    setSelectedPath(path);
    setHighlightLine(line);
  }, []);

  const handleGoTo = useCallback((path: string) => {
    setCwd(path);
    setSelectedPath(null);
    setHighlightLine(null);
  }, []);

  if (!homeDir) {
    return (
      <div className="explorer-panel">
        <div className="explorer-error">
          このインスタンスには作業ディレクトリが設定されていません
        </div>
      </div>
    );
  }

  const destLabel = isRemote ? `${instance.host_alias}:${cwd}` : cwd;

  return (
    <div className="explorer-panel">
      <ExplorerSearchBar
        rawQuery={search.rawQuery}
        setRawQuery={search.setRawQuery}
        glob={search.glob}
        setGlob={search.setGlob}
      />
      {drop.hovering && (
        <div className="explorer-drop-overlay">
          <div className="explorer-drop-message">
            <div className="explorer-drop-icon">⤓</div>
            <div className="explorer-drop-title">
              ドロップで{isRemote ? "リモートへ転送" : "コピー"}
            </div>
            <div className="explorer-drop-dest">→ {destLabel}</div>
          </div>
        </div>
      )}
      {drop.copying && (
        <div className="explorer-drop-overlay explorer-drop-busy">
          <div className="explorer-drop-message">
            <div className="explorer-drop-title">
              {isRemote ? "rsync 転送中…" : "コピー中…"}
            </div>
            <div className="explorer-drop-dest">→ {destLabel}</div>
          </div>
        </div>
      )}
      {download.busy && (
        <div className="explorer-drop-toast">
          <span>ダウンロード中…</span>
        </div>
      )}
      {!download.busy && (download.savedPath || download.error) && (
        <div
          className={`explorer-drop-toast ${download.error ? "error" : "success"}`}
        >
          <span>
            {download.error
              ? `ダウンロード失敗: ${download.error}`
              : `保存しました: ${download.savedPath}`}
          </span>
          <button className="explorer-drop-toast-close" onClick={dismissDownloadToast}>
            ✕
          </button>
        </div>
      )}
      {contextMenu && (
        <ExplorerContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          node={contextMenu.node}
          onDownload={handleDownload}
          onClose={() => setContextMenu(null)}
        />
      )}
      {(drop.lastResult || drop.lastError) && (
        <div
          className={`explorer-drop-toast ${drop.lastError ? "error" : drop.lastResult?.failed.length ? "warn" : "success"}`}
        >
          <span>
            {drop.lastError
              ? `コピー失敗: ${drop.lastError}`
              : drop.lastResult?.failed.length === 0
                ? `${drop.lastResult.copied} 件をコピーしました`
                : `${drop.lastResult?.copied ?? 0} 件成功 / ${drop.lastResult?.failed.length ?? 0} 件失敗`}
          </span>
          <button className="explorer-drop-toast-close" onClick={drop.dismissResult}>
            ✕
          </button>
        </div>
      )}
      <div className="explorer-body">
        <aside className="explorer-left" style={{ width: split.width }}>
          <ExplorerToolbar
            cwd={cwd}
            homeDir={homeDir}
            onGoTo={handleGoTo}
            onRefresh={tree.refreshAll}
          />
          {search.active ? (
            <ExplorerSearchResults
              query={search.query}
              results={search.results}
              loading={search.loading}
              error={search.error}
              onPick={handleSearchPick}
            />
          ) : (
            <ExplorerTree
              childrenByPath={tree.childrenByPath}
              expanded={tree.expanded}
              selectedPath={selectedPath}
              onToggle={tree.toggle}
              onSelectFile={handleSelectFile}
              onContextMenu={isRemote ? handleContextMenu : undefined}
            />
          )}
        </aside>
        <ExplorerSplitter onStartResize={split.startDrag} />
        <main className="explorer-right">
          <PreviewPane
            instanceId={instance.id}
            rootDir={cwd}
            path={selectedPath}
            highlightLine={highlightLine}
          />
        </main>
      </div>
    </div>
  );
}
