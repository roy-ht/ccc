import { useArchiveSync } from "../hooks/useArchiveSync";

/**
 * アーカイブ同期サイクルのステータスライン（最終同期 N 秒前 / 次回 M 秒後）。
 * 1 秒ごとに更新される。Sessions / Memories パネル下部に常駐させる。
 */
export function SyncStatus() {
  const { agoSecs, nextInSecs } = useArchiveSync();
  return (
    <div className="sync-status" title="アーカイブ同期サイクル（リモート取り込み）の状況">
      <span className="sync-dot" aria-hidden />
      {agoSecs == null ? (
        <span>同期待機中…</span>
      ) : (
        <>
          <span>
            同期 <span className="sync-num">{agoSecs}</span> 秒前
          </span>
          <span className="sync-sep">·</span>
          <span>
            次回 <span className="sync-num">{nextInSecs}</span> 秒後
          </span>
        </>
      )}
    </div>
  );
}
