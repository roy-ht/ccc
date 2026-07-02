//! 読み出しクエリ（`ccc-sessions` CLI と将来の UI 向け）。
//!
//! すべて serde 直列化可能な行構造体を返す（`--json` 出力用）。FTS 検索は
//! `messages_fts`（lindera トークナイザ）を BM25 で順位付けし、snippet を返す。

use rusqlite::Connection;
use serde::Serialize;

/// `list` / `recent` 用のセッション 1 行。
#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub instance_name: Option<String>,
    pub kind: Option<String>,
    pub host_alias: Option<String>,
    pub project: Option<String>,
    pub source: Option<String>,
    pub attribution: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub message_count: i64,
    pub summary: Option<String>,
}

/// `search` のヒット 1 件。
#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: i64,
    pub seq: Option<i64>,
    pub ts: Option<i64>,
    pub role: Option<String>,
    pub project: Option<String>,
    pub snippet: String,
}

/// インスタンス単位のセッション検索ヒット（一覧の絞り込み用）。
/// `SessionRow` の各フィールドを展開しつつ、そのセッション内のマッチ件数 `hits` を持つ。
#[derive(Debug, Serialize)]
pub struct SessionHit {
    #[serde(flatten)]
    pub session: SessionRow,
    /// このセッション内で検索語にマッチしたメッセージ数。
    pub hits: i64,
}

/// `show` のメッセージ 1 行。
#[derive(Debug, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub seq: Option<i64>,
    pub ts: Option<i64>,
    pub role: Option<String>,
    pub msg_type: Option<String>,
    pub tool_name: Option<String>,
    pub is_sidechain: bool,
    pub agent_id: Option<String>,
    pub text: Option<String>,
    pub raw: Option<String>,
}

/// `list_sessions` のフィルタ。`None` の項目は無視される。
#[derive(Debug, Default)]
pub struct ListFilter {
    pub kind: Option<String>,
    pub host: Option<String>,
    pub project: Option<String>,
    /// この unix ms 以降に活動があったセッションのみ。
    pub since_ms: Option<i64>,
    pub limit: i64,
}

/// 指定 `session_id` の `summary`（aiTitle 優先・無ければ最初のユーザー入力）を返す。
/// セッション行が存在しない、または summary が NULL の場合は `Ok(None)`。
/// サイドバー「セッションタイトル」表示で hook→ingest 完了後にバックエンドから
/// 引くために使う。Sessions タブで表示される値と同じソース。
pub fn session_summary(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT summary FROM sessions WHERE session_id = ?1",
        rusqlite::params![session_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
}

/// セッション一覧（活動時刻の新しい順）。
pub fn list_sessions(conn: &Connection, f: &ListFilter) -> anyhow::Result<Vec<SessionRow>> {
    let limit = if f.limit <= 0 { 50 } else { f.limit };
    let mut stmt = conn.prepare(
        "SELECT session_id, instance_name, kind, host_alias, project, source, attribution,
                started_at, ended_at, message_count, summary
         FROM sessions
         WHERE (?1 IS NULL OR kind = ?1)
           AND (?2 IS NULL OR host_alias = ?2)
           AND (?3 IS NULL OR project = ?3)
           AND (?4 IS NULL OR COALESCE(ended_at, started_at, 0) >= ?4)
         ORDER BY COALESCE(ended_at, started_at, 0) DESC
         LIMIT ?5",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![f.kind, f.host, f.project, f.since_ms, limit],
            |r| {
                Ok(SessionRow {
                    session_id: r.get(0)?,
                    instance_name: r.get(1)?,
                    kind: r.get(2)?,
                    host_alias: r.get(3)?,
                    project: r.get(4)?,
                    source: r.get(5)?,
                    attribution: r.get(6)?,
                    started_at: r.get(7)?,
                    ended_at: r.get(8)?,
                    message_count: r.get(9)?,
                    summary: r.get(10)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 全文検索（BM25 順）。`query` は lindera で分割される（複数トークンは暗黙 AND）。
pub fn search(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    since_ms: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<SearchHit>> {
    let limit = if limit <= 0 { 30 } else { limit };
    let mut stmt = conn.prepare(
        "SELECT m.session_id, m.id, m.seq, m.ts, m.role, s.project,
                snippet(messages_fts, 0, '《', '》', '…', 12)
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         JOIN sessions s ON s.session_id = m.session_id
         WHERE messages_fts MATCH ?1
           AND (?2 IS NULL OR s.project = ?2)
           AND (?3 IS NULL OR m.ts >= ?3)
         ORDER BY bm25(messages_fts)
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![query, project, since_ms, limit], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                message_id: r.get(1)?,
                seq: r.get(2)?,
                ts: r.get(3)?,
                role: r.get(4)?,
                project: r.get(5)?,
                snippet: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `MessageRow` 用の SELECT 列順。`read_message_row` と必ず一致させること。
const MESSAGE_COLS: &str =
    "id, seq, ts, role, msg_type, tool_name, is_sidechain, agent_id, text, raw, raw_zstd";

/// `MESSAGE_COLS` の順で 1 行を `MessageRow` に読み出す。
///
/// schema v4: 共有辞書は 1 DB に 1 つ（kind='raw'）。辞書ありで圧縮された BLOB は
/// 辞書なし decode に失敗するので、辞書を優先試行 → なければ辞書なし、の順で試す。
///
/// - 辞書 + `raw_zstd` あり: 辞書で decode
/// - `raw_zstd` のみ (辞書未学習 DB 由来 or 初期投入時): 通常 zstd で decode
/// - 旧 `raw` TEXT 列（schema v1 互換）: そのまま返す
///
/// decode 失敗は破損データなので `raw=None` にして UI を死なせない。
fn read_message_row(conn: &Connection, r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
    let raw_text: Option<String> = r.get(9)?;
    let raw_zstd: Option<Vec<u8>> = r.get(10)?;
    let raw = match (raw_zstd, raw_text) {
        (Some(z), _) => {
            // 辞書があれば辞書 decode。なければ / 失敗時は通常 decode を試す。
            let from_dict = crate::dicts::load_decoder(conn, crate::dicts::KIND_RAW)
                .ok()
                .flatten()
                .and_then(|dec| crate::compress::decode_blob_to_string_with_dict(&z, &dec).ok());
            from_dict.or_else(|| crate::compress::decode_blob_to_string(&z).ok())
        }
        (None, Some(t)) => Some(t),
        (None, None) => None,
    };
    Ok(MessageRow {
        id: r.get(0)?,
        seq: r.get(1)?,
        ts: r.get(2)?,
        role: r.get(3)?,
        msg_type: r.get(4)?,
        tool_name: r.get(5)?,
        is_sidechain: r.get::<_, i64>(6)? != 0,
        agent_id: r.get(7)?,
        text: r.get(8)?,
        raw,
    })
}

/// 1 セッションの全メッセージ（順序つき）。
pub fn show_session(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<MessageRow>> {
    let sql = format!(
        "SELECT {MESSAGE_COLS}
         FROM messages WHERE session_id = ?1
         ORDER BY COALESCE(seq, id)"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([session_id], |r| read_message_row(conn, r))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ─── インスタンス単位の絞り込み（ccc 本体の UI 向け） ──────────────────────────
//
// セッションは hook 帰属（instance_id）が付かないことが多い（起動時フルスキャンや
// 孤児取込）。そこで UI では **接続ホスト（or local）＋作業ディレクトリ(cwd)** で
// インスタンスに紐づける。`instance_id` には依存しないため、インスタンスを作り直しても
// 同じディレクトリの履歴を辿れる。

/// 一覧の暴走防止用の上限（内部定数。ユーザー入力ではない）。
const LOCATION_LIMIT: i64 = 2000;

/// セッション内マッチの上限（暴走防止）。一般的な語×巨大セッションの保険。
const MESSAGES_LIMIT: i64 = 1000;

/// `directory`（作業ディレクトリ）と `host_alias` に一致するセッションを絞り込む
/// WHERE 断片と束縛パラメータを組み立てる。
///
/// - ローカルインスタンス（`host_alias = None`）は `host_alias IS NULL` のセッション。
/// - `directory` が `~/` 始まり（リモートの未展開パス）なら末尾一致でも拾う。
/// - `prefix` はテーブル別名（FTS join 時の `"s."` など。素のときは `""`）。
fn location_predicate(
    prefix: &str,
    directory: &str,
    host_alias: Option<&str>,
) -> (String, Vec<String>) {
    let mut params: Vec<String> = Vec::new();
    let mut cwd_cond = format!("{prefix}cwd = ?");
    params.push(directory.to_string());
    if let Some(tail) = directory.strip_prefix("~/") {
        cwd_cond = format!("({cwd_cond} OR {prefix}cwd LIKE ?)");
        params.push(format!("%/{tail}"));
    }
    let host_cond = match host_alias {
        Some(h) => {
            params.push(h.to_string());
            format!("{prefix}host_alias = ?")
        }
        None => format!("{prefix}host_alias IS NULL"),
    };
    (format!("{cwd_cond} AND {host_cond}"), params)
}

/// `messages_fts` の MATCH 用にユーザー入力を安全なクエリ式へ整形する。
///
/// 空白区切りの各語をダブルクォートで括って AND 連結する（`"設計" "会議"`）。
/// これで括弧やハイフン等の FTS5 構文記号による解析エラーを防ぐ。語が無ければ `None`。
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// `SessionRow` を 1 行読み出す（列順は下の SELECT と一致させること）。
fn read_session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: r.get(0)?,
        instance_name: r.get(1)?,
        kind: r.get(2)?,
        host_alias: r.get(3)?,
        project: r.get(4)?,
        source: r.get(5)?,
        attribution: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        message_count: r.get(9)?,
        summary: r.get(10)?,
    })
}

const SESSION_COLS: &str =
    "session_id, instance_name, kind, host_alias, project, source, attribution,
            started_at, ended_at, message_count, summary";

/// インスタンス（host＋cwd）に紐づくセッションを活動の新しい順で返す。
pub fn sessions_for_location(
    conn: &Connection,
    directory: &str,
    host_alias: Option<&str>,
) -> anyhow::Result<Vec<SessionRow>> {
    let (pred, params) = location_predicate("", directory, host_alias);
    let sql = format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE {pred}
         ORDER BY COALESCE(ended_at, started_at, 0) DESC
         LIMIT {LOCATION_LIMIT}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), read_session_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// インスタンスに紐づくセッションを全文検索し、ヒットしたセッションだけを
/// （マッチ件数つきで）活動の新しい順に返す。
pub fn search_sessions_for_location(
    conn: &Connection,
    directory: &str,
    host_alias: Option<&str>,
    query: &str,
) -> anyhow::Result<Vec<SessionHit>> {
    let Some(match_query) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let (pred, loc_params) = location_predicate("s.", directory, host_alias);
    // 束縛順: MATCH 語 → location 述語。
    let mut params: Vec<String> = vec![match_query];
    params.extend(loc_params);
    // 並び順は「ヒット件数の多い順」を主軸にし、同数は活動時刻の新しい順で安定化させる。
    // よく当たるセッションを上に出した方が「検索」UI の期待挙動に近い。
    let sql = format!(
        "SELECT {},
                COUNT(*) AS hits
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         JOIN sessions s ON s.session_id = m.session_id
         WHERE messages_fts MATCH ?
           AND {pred}
         GROUP BY s.session_id
         ORDER BY hits DESC, COALESCE(s.ended_at, s.started_at, 0) DESC
         LIMIT {LOCATION_LIMIT}",
        SESSION_COLS
            .split(',')
            .map(|c| format!("s.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SessionHit {
                session: read_session_row(r)?,
                hits: r.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 1 セッション内で検索語にマッチしたメッセージのみを順序つきで返す。
pub fn search_session_messages(
    conn: &Connection,
    session_id: &str,
    query: &str,
) -> anyhow::Result<Vec<MessageRow>> {
    let Some(match_query) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let prefixed = MESSAGE_COLS
        .split(',')
        .map(|c| format!("m.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    // 暴走防止の上限。一般的な語で巨大セッションを舐めるケースの保険。
    let sql = format!(
        "SELECT {prefixed}
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1 AND m.session_id = ?2
         ORDER BY COALESCE(m.seq, m.id)
         LIMIT {MESSAGES_LIMIT}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![match_query, session_id], |r| {
            read_message_row(conn, r)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `stats` の集計結果。
#[derive(Debug, Serialize)]
pub struct ArchiveStats {
    pub sessions: i64,
    pub messages: i64,
    pub local_sessions: i64,
    pub remote_sessions: i64,
    /// 活動の最古/最新（unix ms）。
    pub first_activity: Option<i64>,
    pub last_activity: Option<i64>,
    /// メモリの異なるファイル数（agent_profile+rel_path）と総版数。
    pub memory_files: i64,
    pub memory_versions: i64,
    /// セッション数の多いプロジェクト上位。
    pub top_projects: Vec<LabeledCount>,
    /// 帰属（hook/inferred/host）別の内訳。
    pub by_attribution: Vec<LabeledCount>,
}

/// ラベル付きカウント（プロジェクト別・帰属別などの内訳行）。
#[derive(Debug, Serialize)]
pub struct LabeledCount {
    pub label: Option<String>,
    pub count: i64,
}

/// アーカイブ全体の統計を集計する。
pub fn stats(conn: &Connection) -> anyhow::Result<ArchiveStats> {
    let one = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
    let opt =
        |sql: &str| -> rusqlite::Result<Option<i64>> { conn.query_row(sql, [], |r| r.get(0)) };

    let top_projects = labeled(
        conn,
        "SELECT project, COUNT(*) FROM sessions
         WHERE project IS NOT NULL
         GROUP BY project ORDER BY COUNT(*) DESC, project LIMIT 5",
    )?;
    let by_attribution = labeled(
        conn,
        "SELECT attribution, COUNT(*) FROM sessions
         GROUP BY attribution ORDER BY COUNT(*) DESC",
    )?;

    Ok(ArchiveStats {
        sessions: one("SELECT COUNT(*) FROM sessions")?,
        messages: one("SELECT COUNT(*) FROM messages")?,
        // ローカル取込のみ（hook/pull で kind 未確定）のセッションは kind=NULL のため
        // local に含める。これで local + remote が総数と一致する（remote は明示値のみ）。
        local_sessions: one("SELECT COUNT(*) FROM sessions WHERE kind='local' OR kind IS NULL")?,
        remote_sessions: one("SELECT COUNT(*) FROM sessions WHERE kind='remote'")?,
        first_activity: opt("SELECT MIN(started_at) FROM sessions")?,
        last_activity: opt("SELECT MAX(COALESCE(ended_at, started_at)) FROM sessions")?,
        memory_files: one("SELECT COUNT(*) FROM (SELECT 1 FROM memory_snapshots
             GROUP BY COALESCE(agent_profile,''), rel_path)")?,
        memory_versions: one("SELECT COUNT(*) FROM memory_snapshots")?,
        top_projects,
        by_attribution,
    })
}

/// `SELECT <label>, COUNT(*)` 形式のクエリを `LabeledCount` 群に変換する。
fn labeled(conn: &Connection, sql: &str) -> anyhow::Result<Vec<LabeledCount>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LabeledCount {
                label: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// プレフィックス（短縮 id）から完全な session_id 群を解決する。完全一致を先頭に。
pub fn resolve_session_ids(conn: &Connection, prefix: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT session_id FROM sessions
         WHERE session_id = ?1 OR session_id LIKE ?1 || '%'
         ORDER BY (session_id = ?1) DESC, session_id
         LIMIT 10",
    )?;
    let ids = stmt
        .query_map([prefix], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Connection {
        let conn = crate::open_in_memory().unwrap();
        // 直接 INSERT で 2 セッション分を投入（ingest 経由でなく素直に）。
        conn.execute(
            "INSERT INTO sessions(session_id, project, started_at, ended_at, message_count, summary)
             VALUES ('s1','ccc',1000,2000,1,'設計レビュー')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, ts, role, text)
             VALUES ('s1','u1',0,1500,'assistant','会議で設計をレビューした')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn list_and_search() {
        let conn = seed();
        let sessions = list_sessions(&conn, &ListFilter::default()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project.as_deref(), Some("ccc"));

        let hits = search(&conn, "会議", None, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert!(hits[0].snippet.contains('《')); // マッチ箇所がマーク

        let msgs = show_session(&conn, "s1").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role.as_deref(), Some("assistant"));
    }

    #[test]
    fn search_respects_project_filter() {
        let conn = seed();
        assert_eq!(
            search(&conn, "会議", Some("other"), None, 10)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            search(&conn, "会議", Some("ccc"), None, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn location_filter_matches_host_and_cwd() {
        let conn = crate::open_in_memory().unwrap();
        // ローカル（host_alias NULL）/ 別ディレクトリ / 同 cwd のリモート を投入。
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at, ended_at) VALUES ('l1','/work/ccc',10,20)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at) VALUES ('l2','/work/other',5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, host_alias, started_at) VALUES ('r1','/work/ccc','myhost',30)",
            [],
        ).unwrap();

        // local: cwd 一致かつ host_alias IS NULL のみ。
        let local = sessions_for_location(&conn, "/work/ccc", None).unwrap();
        assert_eq!(
            local
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["l1"]
        );
        // remote: host_alias 一致かつ cwd 一致。
        let remote = sessions_for_location(&conn, "/work/ccc", Some("myhost")).unwrap();
        assert_eq!(
            remote
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["r1"]
        );
    }

    #[test]
    fn search_scoped_to_location_then_session() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at, ended_at) VALUES ('s1','/w/ccc',10,20)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text) VALUES ('s1','u1',0,'user','設計のレビュー')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text) VALUES ('s1','u2',1,'assistant','別の話題')",
            [],
        ).unwrap();
        // 別ディレクトリにも同じ語があるが location で除外される。
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at) VALUES ('s2','/w/other',5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text) VALUES ('s2','u3',0,'user','設計の話')",
            [],
        ).unwrap();

        let hits = search_sessions_for_location(&conn, "/w/ccc", None, "設計").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.session_id, "s1");
        assert_eq!(hits[0].hits, 1);

        // セッション内検索: ヒットしたメッセージのみ。
        let msgs = search_session_messages(&conn, "s1", "設計").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text.as_deref(), Some("設計のレビュー"));

        // 空クエリは空結果。
        assert!(search_session_messages(&conn, "s1", "   ")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_results_ordered_by_hits_desc() {
        let conn = crate::open_in_memory().unwrap();
        // 同じ cwd に 2 セッション: s_few=1 hit, s_many=3 hits。
        // 活動時刻は s_few の方が新しい（時系列順だけだと逆順になる配置）。
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at, ended_at) VALUES ('s_many','/w/ccc',10,20)",
            [],
        ).unwrap();
        for (i, t) in ["設計の話", "設計のレビュー", "設計と実装"].iter().enumerate() {
            conn.execute(
                "INSERT INTO messages(session_id, uuid, seq, role, text) VALUES ('s_many',?1,?2,'user',?3)",
                rusqlite::params![format!("m{i}"), i as i64, t],
            ).unwrap();
        }
        conn.execute(
            "INSERT INTO sessions(session_id, cwd, started_at, ended_at) VALUES ('s_few','/w/ccc',100,200)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, uuid, seq, role, text) VALUES ('s_few','f1',0,'user','設計だけ一度')",
            [],
        ).unwrap();

        let hits = search_sessions_for_location(&conn, "/w/ccc", None, "設計").unwrap();
        // ヒット数の多い s_many が先頭、s_few が後ろ。
        let ids: Vec<_> = hits.iter().map(|h| h.session.session_id.as_str()).collect();
        assert_eq!(ids, ["s_many", "s_few"]);
        assert_eq!(hits[0].hits, 3);
        assert_eq!(hits[1].hits, 1);
    }

    #[test]
    fn stats_aggregates() {
        let conn = seed();
        // もう 1 セッション（リモート・別プロジェクト）と帰属を足す。
        conn.execute(
            "INSERT INTO sessions(session_id, project, kind, attribution, started_at, ended_at, message_count)
             VALUES ('s2','lab','remote','inferred',3000,4000,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET kind='local', attribution='hook' WHERE session_id='s1'",
            [],
        )
        .unwrap();
        // メモリも 1 ファイル 2 版。
        for (h, c) in [("h1", "v1"), ("h2", "v2")] {
            conn.execute(
                "INSERT INTO memory_snapshots(agent_profile, rel_path, content_hash, content, captured_at)
                 VALUES ('p','CLAUDE.md',?1,?2,1)",
                rusqlite::params![h, c],
            )
            .unwrap();
        }

        let s = stats(&conn).unwrap();
        assert_eq!(s.sessions, 2);
        assert_eq!(s.messages, 1);
        assert_eq!((s.local_sessions, s.remote_sessions), (1, 1));
        assert_eq!(s.first_activity, Some(1000));
        assert_eq!(s.last_activity, Some(4000));
        assert_eq!(s.memory_files, 1, "rel_path 1 種");
        assert_eq!(s.memory_versions, 2);
        assert_eq!(s.top_projects.len(), 2);
        // 帰属内訳に hook と inferred が 1 件ずつ。
        assert_eq!(s.by_attribution.iter().map(|c| c.count).sum::<i64>(), 2);
    }
}
