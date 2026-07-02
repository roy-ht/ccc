//! DBスキーマ定義（v1 〜 v4）と適用。
//!
//! - v1: 初期スキーマ
//! - v2: `messages.raw_zstd` / `events.payload_zstd` BLOB カラム（透過 zstd 圧縮）
//! - v3: `compression_dicts` テーブル + `raw_dict_id` / `payload_dict_id` 列（実装後 v4 に整理）
//! - v4: `compression_dicts` を `kind` PRIMARY KEY に簡素化、`raw_dict_id` /
//!   `payload_dict_id` を削除。**1 DB に 1 辞書 (kind 別)** の不変条件に統一
//!
//! v1 DDL は全て冪等 (`IF NOT EXISTS`) で、接続オープンごとに流しても安全。
//! v2 以降の列追加は `PRAGMA table_info` で存在チェックしてから実行する。
//! v3→v4 の構造変更は専用のマイグレーション関数で対応する。
//!
//! 注意: `messages_fts` は `tokenize='lindera'` を使うため、この DDL を流す前に
//! 自前 lindera トークナイザを接続に登録しておく必要がある（[`crate::fts`]）。

use rusqlite::Connection;

/// 現在のスキーマバージョン。破壊的変更のたびに +1 し migrate を追加する。
pub const SCHEMA_VERSION: i64 = 4;

/// 現行（v4）の全 DDL。`messages_fts` の external-content 同期はトリガで行う。
/// 新規 DB はこの DDL 1 枚で完成する。既存 DB は冪等な `IF NOT EXISTS` で済む部分は
/// そのまま、構造変更が必要な部分は `migrate` 内のステップで対応する。
const DDL: &str = r#"
-- インスタンス台帳（~/.ccc/instances から消えても素性を残す）
CREATE TABLE IF NOT EXISTS instances (
  instance_id   TEXT PRIMARY KEY,
  name          TEXT,
  instance_hash TEXT,
  kind          TEXT,            -- 'local' | 'remote'
  host_alias    TEXT,
  agent_profile TEXT,
  directory     TEXT,            -- 起動 cwd（推定補完のアンカー）
  first_seen    INTEGER,
  last_seen     INTEGER
);

-- セッション（1 Claude Code session = 1 行）
CREATE TABLE IF NOT EXISTS sessions (
  session_id    TEXT PRIMARY KEY,
  instance_id   TEXT,
  instance_name TEXT,            -- 取得元インスタンス名（非正規化）
  attribution   TEXT,            -- 'hook' | 'inferred' | 'host'
  kind          TEXT,
  host_alias    TEXT,
  agent_profile TEXT,
  cwd           TEXT,
  project       TEXT,
  source        TEXT,            -- startup|resume|clear|compact（hook 由来。不明は NULL）
  started_at    INTEGER,
  ended_at      INTEGER,
  summary       TEXT,
  message_count INTEGER NOT NULL DEFAULT 0
);

-- メッセージ（transcript の 1 行 = 1 行）
CREATE TABLE IF NOT EXISTS messages (
  id           INTEGER PRIMARY KEY,
  session_id   TEXT NOT NULL,
  uuid         TEXT,             -- transcript 行の uuid（冪等キー）
  parent_uuid  TEXT,
  seq          INTEGER,
  ts           INTEGER,
  role         TEXT,
  msg_type     TEXT,
  tool_name    TEXT,
  is_sidechain INTEGER NOT NULL DEFAULT 0,
  agent_id     TEXT,             -- subagent 会話の agentId（本体は NULL）
  text         TEXT,
  raw          TEXT,
  UNIQUE(session_id, uuid)
);

-- 全文検索（external content + 自前 lindera トークナイザ）
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  text,
  content='messages',
  content_rowid='id',
  tokenize='lindera'
);

-- external-content 同期トリガ
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

-- 増分取り込みカーソル（ファイル単位）
CREATE TABLE IF NOT EXISTS ingest_cursors (
  file_path   TEXT PRIMARY KEY,
  session_id  TEXT,
  last_offset INTEGER NOT NULL DEFAULT 0,
  last_size   INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER
);

-- メモリスナップショット（内容ハッシュで重複排除）
CREATE TABLE IF NOT EXISTS memory_snapshots (
  id            INTEGER PRIMARY KEY,
  agent_profile TEXT,
  rel_path      TEXT,
  scope         TEXT,            -- 'user' | 'project'
  project       TEXT,
  content_hash  TEXT,
  content       TEXT,
  source_kind   TEXT,            -- 'local' | 'remote'
  host_alias    TEXT,
  captured_at   INTEGER,
  UNIQUE(agent_profile, rel_path, content_hash)
);

-- hook イベントログ（活動再構成・推定補完の時間窓に使用）
CREATE TABLE IF NOT EXISTS events (
  id          INTEGER PRIMARY KEY,
  instance_id TEXT,
  session_id  TEXT,
  hook_event  TEXT,
  ts          INTEGER,
  payload     TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_session_seq ON messages(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project);
CREATE INDEX IF NOT EXISTS idx_events_instance_ts ON events(instance_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);

CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT);

-- v4: 共有 zstd 辞書（**1 DB に 1 辞書 (kind 別)**）。
-- 再学習時は同 kind の行を UPSERT で上書きし、`messages.raw_zstd` / `events.payload_zstd`
-- 全行を新辞書で atomic に再圧縮する（dict_id 列は持たない）。
-- シャーディング時は DB ごとに独立した辞書を持てるため、過去 DB を触らず最新だけ再学習できる。
CREATE TABLE IF NOT EXISTS compression_dicts (
  kind          TEXT PRIMARY KEY,
  dict_blob     BLOB NOT NULL,
  capacity      INTEGER,
  sample_count  INTEGER,
  sample_bytes  INTEGER,
  created_at    INTEGER NOT NULL
);
"#;

/// スキーマを適用し、`schema_meta.version` を最新化する。
///
/// 段階:
/// 1. `DDL` を流す（IF NOT EXISTS で冪等）
/// 2. v2 追加列を `PRAGMA table_info` で存在チェックして ALTER
/// 3. v3→v4 構造変更（旧 `compression_dicts` を新形式に置換、`*_dict_id` 列を削除）
/// 4. `schema_meta.version` を `SCHEMA_VERSION` に更新
pub fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(DDL)?;

    // v2: zstd BLOB 列
    if !column_exists(conn, "messages", "raw_zstd")? {
        conn.execute("ALTER TABLE messages ADD COLUMN raw_zstd BLOB", [])?;
    }
    if !column_exists(conn, "events", "payload_zstd")? {
        conn.execute("ALTER TABLE events ADD COLUMN payload_zstd BLOB", [])?;
    }

    // v3→v4: compression_dicts を新形式に + dict_id 列を削除
    upgrade_compression_dicts_to_v4(conn)?;
    drop_legacy_dict_id_columns(conn)?;

    conn.execute(
        "INSERT INTO schema_meta(key, value) VALUES ('version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// 旧 `compression_dicts(id PRIMARY KEY, kind, ..., is_active)` を
/// 新 `compression_dicts(kind PRIMARY KEY, ...)` に置換する。
///
/// 旧 DB に id 列があれば実行。`is_active=1` の行のみを新テーブルに移し、それ以外（旧辞書）
/// は捨てる。**この時点で `messages.raw_zstd` / `events.payload_zstd` は現アクティブ辞書で
/// 圧縮されている前提**（v3 期の `train-dict` は全行を再圧縮していたため整合性は保たれる）。
fn upgrade_compression_dicts_to_v4(conn: &Connection) -> anyhow::Result<()> {
    if !column_exists(conn, "compression_dicts", "id")? {
        return Ok(());
    }
    // 退避: kind × dict_blob × meta（is_active=1 の行のみ）
    type Row = (String, Vec<u8>, Option<i64>, Option<i64>, Option<i64>, i64);
    let actives: Vec<Row> = {
        let mut stmt = conn.prepare(
            "SELECT kind, dict_blob, capacity, sample_count, sample_bytes, created_at
             FROM compression_dicts WHERE is_active = 1",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };

    let tx = conn.unchecked_transaction()?;
    tx.execute("DROP TABLE compression_dicts", [])?;
    tx.execute(
        "CREATE TABLE compression_dicts (
            kind          TEXT PRIMARY KEY,
            dict_blob     BLOB NOT NULL,
            capacity      INTEGER,
            sample_count  INTEGER,
            sample_bytes  INTEGER,
            created_at    INTEGER NOT NULL
        )",
        [],
    )?;
    for (kind, dict_blob, capacity, sample_count, sample_bytes, created_at) in actives {
        tx.execute(
            "INSERT INTO compression_dicts(kind, dict_blob, capacity, sample_count, sample_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![kind, dict_blob, capacity, sample_count, sample_bytes, created_at],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 旧 `raw_dict_id` / `payload_dict_id` 列を DROP する（SQLite 3.35+）。
fn drop_legacy_dict_id_columns(conn: &Connection) -> anyhow::Result<()> {
    if column_exists(conn, "messages", "raw_dict_id")? {
        conn.execute("ALTER TABLE messages DROP COLUMN raw_dict_id", [])?;
    }
    if column_exists(conn, "events", "payload_dict_id")? {
        conn.execute("ALTER TABLE events DROP COLUMN payload_dict_id", [])?;
    }
    Ok(())
}

/// PRAGMA table_info でカラムの存在を判定する。`table` は内部呼び出し限定の安全な定数のみ受ける。
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_in_memory;

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn v4_has_zstd_columns_but_no_dict_id_columns() {
        let conn = open_in_memory().unwrap();
        let m = columns(&conn, "messages");
        assert!(m.iter().any(|c| c == "raw_zstd"));
        assert!(
            !m.iter().any(|c| c == "raw_dict_id"),
            "v4: raw_dict_id は無いはず"
        );
        let e = columns(&conn, "events");
        assert!(e.iter().any(|c| c == "payload_zstd"));
        assert!(
            !e.iter().any(|c| c == "payload_dict_id"),
            "v4: payload_dict_id は無いはず"
        );

        let ver: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION.to_string());

        // compression_dicts は kind PRIMARY KEY
        let pk_kind: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('compression_dicts') WHERE name='kind' AND pk=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pk_kind, 1, "compression_dicts.kind が PRIMARY KEY");
        assert_eq!(
            columns(&conn, "compression_dicts")
                .iter()
                .filter(|c| *c == "id")
                .count(),
            0,
            "v4: 旧 id 列は無いはず"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn v3_to_v4_keeps_active_dict_and_drops_others() {
        // v4 DB を作ってから、わざと旧 v3 状態に戻して migrate を再実行する。
        // これで「v3 DB を v4 に上げるパス」を実環境に近い形でテストする。
        let conn = open_in_memory().unwrap();
        // v4 → 模擬 v3 への巻き戻し:
        // - compression_dicts を id PRIMARY KEY + is_active 形式に置き換え
        // - messages.raw_dict_id / events.payload_dict_id を再追加
        conn.execute_batch(
            "DROP TABLE compression_dicts;
             CREATE TABLE compression_dicts(
               id INTEGER PRIMARY KEY, kind TEXT NOT NULL, dict_blob BLOB NOT NULL,
               capacity INTEGER, sample_count INTEGER, sample_bytes INTEGER,
               created_at INTEGER NOT NULL, is_active INTEGER NOT NULL DEFAULT 0
             );
             ALTER TABLE messages ADD COLUMN raw_dict_id INTEGER;
             ALTER TABLE events ADD COLUMN payload_dict_id INTEGER;
             UPDATE schema_meta SET value='3' WHERE key='version';",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO compression_dicts(kind, dict_blob, capacity, sample_count, sample_bytes, created_at, is_active)
             VALUES ('raw', x'AABB', 110000, 100, 50000, 1, 1),
                    ('payload', x'CCDD', 32000, 50, 10000, 2, 1),
                    ('raw', x'1111', 110000, 10, 100, 0, 0)",
            [],
        )
        .unwrap();

        // v4 migrate を流す
        migrate(&conn).unwrap();

        // 結果: is_active=1 だった行だけ kind ごと 1 件残る、id 列は消える
        let cnt: i64 = conn
            .query_row("SELECT COUNT(*) FROM compression_dicts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 2);
        let raw_blob: Vec<u8> = conn
            .query_row(
                "SELECT dict_blob FROM compression_dicts WHERE kind='raw'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw_blob, vec![0xAA, 0xBB]);
        assert!(!columns(&conn, "messages")
            .iter()
            .any(|c| c == "raw_dict_id"));
        assert!(!columns(&conn, "events")
            .iter()
            .any(|c| c == "payload_dict_id"));
        let ver: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, "4");
    }
}
