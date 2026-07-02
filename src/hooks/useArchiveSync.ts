import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** `archive_sync_status` コマンドの返り。 */
interface SyncStatus {
  last_at: number | null;
  interval_secs: number;
}

/** `archive-sync` イベントのペイロード（同期サイクルのハートビート）。 */
interface SyncTick {
  at: number;
  interval_secs: number;
  pulled_hosts: number;
}

export interface ArchiveSyncInfo {
  /** 最終同期からの経過秒（未同期なら null）。 */
  agoSecs: number | null;
  /** 次回同期までの秒（未同期なら null）。 */
  nextInSecs: number | null;
  /** 同期サイクルの間隔（秒）。 */
  intervalSecs: number;
}

/**
 * アーカイブ（セッション/メモリ）の同期サイクルの状況を秒刻みで返す。
 *
 * 初期値は `archive_sync_status` コマンドで取得し、以降は `archive-sync` イベントで
 * 最終同期時刻を更新する。経過秒・残り秒はクライアント側で 1 秒ごとに再計算する
 * （デジタル時計のように更新される）。
 */
export function useArchiveSync(): ArchiveSyncInfo {
  const [lastAt, setLastAt] = useState<number | null>(null);
  const [intervalSecs, setIntervalSecs] = useState<number>(60);
  // 1 秒ごとに再描画して経過/残り秒を更新するためのティック。
  const [, setTick] = useState(0);

  useEffect(() => {
    let cancelled = false;

    invoke<SyncStatus>("archive_sync_status")
      .then((s) => {
        if (cancelled) return;
        setLastAt(s.last_at);
        setIntervalSecs(s.interval_secs);
      })
      .catch(() => {
        /* 集約無効などでも無視（ステータスは未同期表示のまま） */
      });

    const unlisten = listen<SyncTick>("archive-sync", (e) => {
      setLastAt(e.payload.at);
      setIntervalSecs(e.payload.interval_secs);
    });

    const timer = window.setInterval(() => setTick((n) => n + 1), 1000);

    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
      window.clearInterval(timer);
    };
  }, []);

  if (lastAt == null) {
    return { agoSecs: null, nextInSecs: null, intervalSecs };
  }
  const ago = Math.max(0, Math.floor((Date.now() - lastAt) / 1000));
  const nextIn = Math.max(0, intervalSecs - ago);
  return { agoSecs: ago, nextInSecs: nextIn, intervalSecs };
}
