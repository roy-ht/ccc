//! セッション/メモリ集約 DB の読み出しコマンド（UI の Sessions / Memories 画面向け）。
//!
//! 書き込みは `ArchiveService`（単一 writer スレッド）が担う。ここでは別途 **読み取り用の
//! 接続を 1 本キャッシュ**し（`ccc_archive::open` は接続ごとに 18MB の IPADIC 辞書を
//! ロードするため、検索のたびに開くと重い）、`Mutex` で直列化して UI からのクエリに応える。
//! WAL モードなので writer と同時並行に読める。
//!
//! セッションの帰属は hook（instance_id）に頼らず **接続ホスト(or local)＋作業ディレクトリ**
//! で行う（`sessions_for_location`）。メモリは **プロファイル＋当該プロジェクト**に閉じる。
//! いずれも横断検索ではなく、選択中インスタンスのデータに閉じた絞り込みである。

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use ccc_archive::{Connection, MemoryEntry, MessageRow, SessionHit, SessionRow};
use serde::Serialize;
use tauri::State;

use crate::archive_service::{LastSyncAt, SYNC_INTERVAL_SECS};

/// UI 読み出し用の接続ハンドル。`None` は DB を開けなかった場合（集約無効）。
pub struct ArchiveDb(pub Mutex<Option<Connection>>);

impl ArchiveDb {
    /// 指定パスの DB を読み取り用に開く。失敗しても `None` を保持し、起動は止めない。
    pub fn open(path: &Path) -> Self {
        match ccc_archive::open(path) {
            Ok(conn) => ArchiveDb(Mutex::new(Some(conn))),
            Err(e) => {
                eprintln!(
                    "[ccc] archive 読み取り接続のオープンに失敗（Sessions/Memories は空）: {e}"
                );
                ArchiveDb(Mutex::new(None))
            }
        }
    }

    /// 集約が無効なときのプレースホルダ。
    pub fn none() -> Self {
        ArchiveDb(Mutex::new(None))
    }
}

/// ロックを取り、接続が生きていれば `f` を実行する。未接続なら専用エラー。
fn with_conn<T>(
    db: &State<'_, ArchiveDb>,
    f: impl FnOnce(&Connection) -> anyhow::Result<T>,
) -> Result<T, String> {
    let guard =
        db.0.lock()
            .map_err(|_| "archive lock poisoned".to_string())?;
    match guard.as_ref() {
        Some(conn) => f(conn).map_err(|e| e.to_string()),
        None => Err("archive DB が利用できません".to_string()),
    }
}

/// Claude Code が `projects/<encoded-cwd>` でディレクトリ名に使う符号化を再現する。
/// cwd の `/` と `.` を `-` に置換する（`-` はそのまま）。
/// 例: `/Users/h/mydocs/ccc` → `-Users-h-mydocs-ccc`。
fn encode_cwd(dir: &str) -> String {
    dir.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// 空白だけ/空文字の検索語を `None` に正規化する。
fn normalize_query(query: Option<String>) -> Option<String> {
    query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
}

/// インスタンス（host＋cwd）に紐づくセッション一覧（活動の新しい順）。
#[tauri::command]
pub fn archive_list_sessions(
    directory: String,
    host_alias: Option<String>,
    db: State<'_, ArchiveDb>,
) -> Result<Vec<SessionRow>, String> {
    with_conn(&db, |conn| {
        ccc_archive::sessions_for_location(conn, &directory, host_alias.as_deref())
    })
}

/// インスタンスに紐づくセッションを全文検索し、ヒットしたものだけを返す。
#[tauri::command]
pub fn archive_search_sessions(
    directory: String,
    host_alias: Option<String>,
    query: String,
    db: State<'_, ArchiveDb>,
) -> Result<Vec<SessionHit>, String> {
    with_conn(&db, |conn| {
        ccc_archive::search_sessions_for_location(conn, &directory, host_alias.as_deref(), &query)
    })
}

/// 1 セッションのメッセージ。`query` 指定時はヒットしたメッセージのみ（順序つき）。
#[tauri::command]
pub fn archive_session_messages(
    session_id: String,
    query: Option<String>,
    db: State<'_, ArchiveDb>,
) -> Result<Vec<MessageRow>, String> {
    let query = normalize_query(query);
    with_conn(&db, |conn| match query.as_deref() {
        Some(q) => ccc_archive::search_session_messages(conn, &session_id, q),
        None => ccc_archive::show_session(conn, &session_id),
    })
}

/// インスタンス（プロファイル＋当該プロジェクト）のメモリ一覧（新しい順）。
/// `query` 指定時は rel_path / content の部分一致で絞り込む。
#[tauri::command]
pub fn archive_list_memory(
    directory: String,
    agent_profile: String,
    query: Option<String>,
    db: State<'_, ArchiveDb>,
) -> Result<Vec<MemoryEntry>, String> {
    let encoded = encode_cwd(&directory);
    let query = normalize_query(query);
    with_conn(&db, |conn| {
        ccc_archive::list_memory_for_instance(conn, &agent_profile, &encoded, query.as_deref())
    })
}

/// UI の同期ステータスライン用に共有する「最終同期 unix ms」ハンドル。
pub struct ArchiveSyncClock(pub LastSyncAt);

/// 同期ステータスの現在値（`archive_sync_status` の返り）。
#[derive(Serialize)]
pub struct SyncStatus {
    /// 最終同期サイクルの時刻（unix ms）。未実施なら `None`。
    pub last_at: Option<i64>,
    /// 次サイクルまでの想定間隔（秒）。
    pub interval_secs: u64,
}

/// 同期ステータスの現在値を返す（UI がマウント時に初期表示するために使う）。
/// 以降の更新は `archive-sync` イベントで配信される。
#[tauri::command]
pub fn archive_sync_status(clock: State<'_, ArchiveSyncClock>) -> SyncStatus {
    let v = clock.0.load(Ordering::Relaxed);
    SyncStatus {
        last_at: if v == 0 { None } else { Some(v) },
        interval_secs: SYNC_INTERVAL_SECS,
    }
}

/// メモリファイルの最新版本文。
#[tauri::command]
pub fn archive_memory_content(
    agent_profile: String,
    rel_path: String,
    db: State<'_, ArchiveDb>,
) -> Result<Option<String>, String> {
    with_conn(&db, |conn| {
        ccc_archive::memory_latest_content(conn, &agent_profile, &rel_path)
    })
}
