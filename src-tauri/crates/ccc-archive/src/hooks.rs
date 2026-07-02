//! hook イベント由来のメタ確定（ccc 本体の ArchiveService から呼ぶ）。
//!
//! - `record_event`: 全 hook を `events` に追記し、`instances` の観測活動窓
//!   （first_seen/last_seen）を更新する（Phase 3 の推定補完が使う）。
//! - `record_session_start`: `SessionStart` で `instances` 台帳と `sessions` メタを
//!   確定し、`attribution='hook'`（最も信頼できる帰属）を立てる。

use rusqlite::Connection;
use serde_json::Value;

/// hook 受信時に分かる ccc 固有のインスタンス情報スナップショット。
#[derive(Debug, Clone, Default)]
pub struct InstanceMeta {
    pub instance_id: String,
    pub name: Option<String>,
    pub kind: Option<String>, // "local" | "remote"
    pub host_alias: Option<String>,
    pub agent_profile: Option<String>,
    pub directory: Option<String>,
}

/// hook 1 件を `events` に追記し、インスタンスの観測活動窓を更新する。
pub fn record_event(
    conn: &Connection,
    instance_id: &str,
    session_id: Option<&str>,
    hook_event: &str,
    ts_ms: i64,
    payload: &Value,
) -> anyhow::Result<()> {
    // schema v2 以降: `payload` には書かず、zstd 圧縮した BLOB を `payload_zstd` に積む。
    // schema v4 以降: 共有辞書があればそれで圧縮（1 DB に 1 辞書 = dict_id 列なし）。
    let payload_str = payload.to_string();
    let payload_zstd = match crate::dicts::load_encoder(conn, crate::dicts::KIND_PAYLOAD)? {
        Some(enc) => crate::compress::encode_str_with_dict(&payload_str, &enc)?,
        None => crate::compress::encode_str(&payload_str)?,
    };
    conn.execute(
        "INSERT INTO events(instance_id, session_id, hook_event, ts, payload_zstd)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![instance_id, session_id, hook_event, ts_ms, payload_zstd],
    )?;
    conn.execute(
        "INSERT INTO instances(instance_id, first_seen, last_seen) VALUES (?1, ?2, ?2)
         ON CONFLICT(instance_id) DO UPDATE SET
           first_seen = MIN(COALESCE(first_seen, ?2), ?2),
           last_seen  = MAX(COALESCE(last_seen, ?2), ?2)",
        rusqlite::params![instance_id, ts_ms],
    )?;
    Ok(())
}

/// `SessionStart` 受信時に台帳・セッションメタを確定する。
pub fn record_session_start(
    conn: &Connection,
    m: &InstanceMeta,
    session_id: &str,
    source: Option<&str>,
    ts_ms: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO instances
           (instance_id, name, kind, host_alias, agent_profile, directory, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(instance_id) DO UPDATE SET
           name          = COALESCE(excluded.name, name),
           kind          = COALESCE(excluded.kind, kind),
           host_alias    = COALESCE(excluded.host_alias, host_alias),
           agent_profile = COALESCE(excluded.agent_profile, agent_profile),
           directory     = COALESCE(excluded.directory, directory),
           last_seen     = MAX(COALESCE(last_seen, ?7), ?7)",
        rusqlite::params![
            m.instance_id,
            m.name,
            m.kind,
            m.host_alias,
            m.agent_profile,
            m.directory,
            ts_ms
        ],
    )?;
    // cwd/project はインスタンスの起動ディレクトリで即時に仮充填する
    // （transcript 取り込み前でも UI のインスタンス紐付けに乗る）。
    // 既に transcript 由来の起点 cwd が入っていればそちらを温存する。
    let directory = m.directory.as_deref();
    let project = directory.and_then(crate::ingest::derive_project);
    conn.execute(
        "INSERT INTO sessions
           (session_id, instance_id, instance_name, attribution, kind, host_alias, agent_profile, source, started_at, cwd, project)
         VALUES (?1, ?2, ?3, 'hook', ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(session_id) DO UPDATE SET
           instance_id   = COALESCE(excluded.instance_id, instance_id),
           instance_name = COALESCE(excluded.instance_name, instance_name),
           attribution   = 'hook',
           kind          = COALESCE(excluded.kind, kind),
           host_alias    = COALESCE(excluded.host_alias, host_alias),
           agent_profile = COALESCE(excluded.agent_profile, agent_profile),
           source        = COALESCE(excluded.source, source),
           started_at    = COALESCE(started_at, excluded.started_at),
           cwd           = COALESCE(cwd, excluded.cwd),
           project       = COALESCE(project, excluded.project)",
        rusqlite::params![
            session_id,
            m.instance_id,
            m.name,
            m.kind,
            m.host_alias,
            m.agent_profile,
            source,
            ts_ms,
            directory,
            project
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_start_sets_hook_attribution() {
        let conn = crate::open_in_memory().unwrap();
        let meta = InstanceMeta {
            instance_id: "i1".into(),
            name: Some("my-ec2".into()),
            kind: Some("remote".into()),
            host_alias: Some("ec2-training".into()),
            agent_profile: Some("default".into()),
            directory: Some("/home/u/proj".into()),
        };
        record_event(
            &conn,
            "i1",
            Some("s1"),
            "SessionStart",
            1000,
            &json!({"a":1}),
        )
        .unwrap();
        record_session_start(&conn, &meta, "s1", Some("startup"), 1000).unwrap();

        let (name, attr, kind): (String, String, String) = conn
            .query_row(
                "SELECT instance_name, attribution, kind FROM sessions WHERE session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "my-ec2");
        assert_eq!(attr, "hook");
        assert_eq!(kind, "remote");

        // cwd/project はインスタンスの起動ディレクトリで即時に仮充填される。
        let (cwd, project): (String, String) = conn
            .query_row(
                "SELECT cwd, project FROM sessions WHERE session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cwd, "/home/u/proj");
        assert_eq!(project, "proj");

        // 台帳に directory（推定補完アンカー）と活動窓が入る
        let (dir, first, last): (String, i64, i64) = conn
            .query_row(
                "SELECT directory, first_seen, last_seen FROM instances WHERE instance_id='i1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(dir, "/home/u/proj");
        assert_eq!((first, last), (1000, 1000));
    }
}
