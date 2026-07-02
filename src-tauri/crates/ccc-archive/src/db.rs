//! DB 接続のオープン。WAL・busy_timeout・lindera トークナイザ登録・スキーマ適用を一括で行う。
//!
//! ccc 本体も `ccc-sessions` CLI もここを通して接続する。これにより
//! 「FTS5 トークナイザを毎接続で登録する」「同一スキーマを保証する」が共通化される。

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::{fts, schema};

/// プロセス間の書き込み競合に備えた待ち時間（§7）。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 指定パスの archive DB を開く（無ければ作成）。
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    Ok(conn)
}

/// テスト用のインメモリ DB を開く。
pub fn open_in_memory() -> anyhow::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    Ok(conn)
}

/// 既存接続に WAL/busy_timeout/トークナイザ/スキーマを適用する共通処理。
///
/// 順序が重要: `messages_fts` は `tokenize='lindera'` を使うため、スキーマ適用の
/// 前に [`fts::register`] を済ませる。
fn configure(conn: &Connection) -> anyhow::Result<()> {
    // :memory: では WAL は無視されるが害は無い。
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    fts::register(conn)?;
    schema::migrate(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// in-process lindera トークナイザ + スキーマ + FTS 同期トリガが一気通貫で動くこと。
    /// 2 字熟語 '設計'/'会議'（unicode61/trigram では引けない）がヒットすることを固定する。
    #[test]
    fn japanese_fts_roundtrip() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES ('s1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text)
             VALUES ('s1', 'u1', 0, 'assistant', '設計のレビューを会議で決めた')",
            [],
        )
        .unwrap();

        for q in ["設計", "会議", "レビュー"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [q],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "MATCH '{q}' が 1 件ヒットするはず");
        }
    }

    /// 削除トリガで FTS インデックスも同期されること。
    #[test]
    fn delete_syncs_fts() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES ('s1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text)
             VALUES ('s1', 'u1', 0, 'assistant', '設計レビュー')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM messages WHERE uuid='u1'", [])
            .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                ["設計"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 冪等性: 同一 (session_id, uuid) の二重取り込みは弾かれる。
    #[test]
    fn upsert_idempotent_by_uuid() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES ('s1')", [])
            .unwrap();
        let sql = "INSERT OR IGNORE INTO messages(session_id, uuid, seq, role, text)
                   VALUES ('s1', 'u1', 0, 'user', 'hello')";
        conn.execute(sql, []).unwrap();
        conn.execute(sql, []).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
