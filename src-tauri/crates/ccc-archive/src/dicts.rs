//! 共有 zstd 辞書の永続化・学習・キャッシュ（schema v4）。
//!
//! **1 DB に 1 辞書 (kind 別)** が不変条件。`compression_dicts` は `kind` PRIMARY KEY で、
//! 再学習時は UPSERT で上書きし、`messages.raw_zstd` / `events.payload_zstd` 全行を
//! 新辞書で atomic に再圧縮する（dict_id 列は持たない）。
//!
//! シャーディング時は DB ごとに別辞書を持てるため、過去 DB は触らず最新だけ再学習できる。
//!
//! Encoder/Decoder の生成（`*Dictionary::copy`）は内部で CDict/DDict を作るので
//! プロセス内 `OnceLock<Mutex<HashMap>>` に kind キーでキャッシュする。再学習時は
//! `invalidate_cache(kind)` で当該 kind のエントリを落として再ロードさせる。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension};
use zstd::dict::{DecoderDictionary, EncoderDictionary};

/// 辞書の用途。`messages.raw` 用と `events.payload` 用で別辞書にする
/// （内容構造が大きく違うため、共通辞書より分離した方が圧縮率が伸びる）。
pub const KIND_RAW: &str = "raw";
pub const KIND_PAYLOAD: &str = "payload";

static DECODER_CACHE: OnceLock<Mutex<HashMap<String, Arc<DecoderDictionary<'static>>>>> =
    OnceLock::new();
static ENCODER_CACHE: OnceLock<Mutex<HashMap<String, Arc<EncoderDictionary<'static>>>>> =
    OnceLock::new();

fn decoder_cache() -> &'static Mutex<HashMap<String, Arc<DecoderDictionary<'static>>>> {
    DECODER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn encoder_cache() -> &'static Mutex<HashMap<String, Arc<EncoderDictionary<'static>>>> {
    ENCODER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 指定 kind の現アクティブ辞書 blob を返す。辞書未学習なら `None`。
fn load_blob(conn: &Connection, kind: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT dict_blob FROM compression_dicts WHERE kind = ?1",
        [kind],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .optional()
}

/// 指定 kind の `EncoderDictionary` を取得。辞書がなければ `None`。キャッシュ済みならそれを返す。
pub fn load_encoder(
    conn: &Connection,
    kind: &str,
) -> anyhow::Result<Option<Arc<EncoderDictionary<'static>>>> {
    if let Some(d) = encoder_cache().lock().unwrap().get(kind) {
        return Ok(Some(d.clone()));
    }
    let Some(blob) = load_blob(conn, kind)? else {
        return Ok(None);
    };
    let dict = Arc::new(EncoderDictionary::copy(&blob, crate::compress::LEVEL));
    encoder_cache()
        .lock()
        .unwrap()
        .insert(kind.to_string(), dict.clone());
    Ok(Some(dict))
}

/// 指定 kind の `DecoderDictionary` を取得。辞書がなければ `None`。
pub fn load_decoder(
    conn: &Connection,
    kind: &str,
) -> anyhow::Result<Option<Arc<DecoderDictionary<'static>>>> {
    if let Some(d) = decoder_cache().lock().unwrap().get(kind) {
        return Ok(Some(d.clone()));
    }
    let Some(blob) = load_blob(conn, kind)? else {
        return Ok(None);
    };
    let dict = Arc::new(DecoderDictionary::copy(&blob));
    decoder_cache()
        .lock()
        .unwrap()
        .insert(kind.to_string(), dict.clone());
    Ok(Some(dict))
}

/// 指定 kind のキャッシュを落とす（再学習で辞書が変わったとき）。
pub fn invalidate_cache(kind: &str) {
    if let Some(m) = ENCODER_CACHE.get() {
        m.lock().unwrap().remove(kind);
    }
    if let Some(m) = DECODER_CACHE.get() {
        m.lock().unwrap().remove(kind);
    }
}

/// 学習結果のサマリ。
#[derive(Debug, Clone, Copy)]
pub struct TrainedDict {
    pub blob_len: usize,
    pub sample_count: usize,
    pub sample_bytes: usize,
}

/// サンプル列から辞書を学習し、`compression_dicts` に **UPSERT** する。
/// 同 kind の旧辞書はこの呼び出しで完全に置き換えられる（履歴は持たない）。
/// **既存データはこの関数では再圧縮されない**ので、呼び出し側は事前に旧 decoder を
/// 取得しておき、`recompress_all_with_new_dict` で全行を新辞書に変換すること。
pub fn upsert_trained_dict(
    conn: &Connection,
    kind: &str,
    dict_blob: &[u8],
    capacity: usize,
    sample_count: usize,
    sample_bytes: usize,
    now_ms: i64,
) -> anyhow::Result<TrainedDict> {
    conn.execute(
        "INSERT INTO compression_dicts(kind, dict_blob, capacity, sample_count, sample_bytes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(kind) DO UPDATE SET
           dict_blob    = excluded.dict_blob,
           capacity     = excluded.capacity,
           sample_count = excluded.sample_count,
           sample_bytes = excluded.sample_bytes,
           created_at   = excluded.created_at",
        rusqlite::params![
            kind,
            dict_blob,
            capacity as i64,
            sample_count as i64,
            sample_bytes as i64,
            now_ms
        ],
    )?;
    invalidate_cache(kind);
    Ok(TrainedDict {
        blob_len: dict_blob.len(),
        sample_count,
        sample_bytes,
    })
}

/// サンプル抽出 → 辞書学習 → 既存全行を再圧縮 → UPSERT を 1 ステップで行う高水準 API。
/// 旧辞書がある場合は **旧 decoder で読み取り**、新 encoder で書き直す。
///
/// バッチ COMMIT で進捗を出すが、UPSERT は **最後**に行う。途中で中断した場合、新フォーマット
/// 行と旧フォーマット行が混在するが、`compression_dicts` はまだ旧辞書のままなので、
/// 起動時の decode は旧辞書で行われ、新フォーマット行は decode 失敗（`raw=None`）になる。
/// 再度 `retrain_kind` を実行すれば整合性が回復する。
pub fn retrain_kind(
    conn: &Connection,
    kind: &str,
    samples_for_training: &[&[u8]],
    capacity: usize,
    batch: usize,
    now_ms: i64,
    mut on_progress: impl FnMut(RecompressProgress),
) -> anyhow::Result<RetrainStats> {
    if samples_for_training.is_empty() {
        anyhow::bail!("学習サンプルが空");
    }

    // 旧 decoder（あれば）を手元に確保しておく。新 UPSERT 前に取らないと差し替え後にロードできない。
    let old_decoder = load_decoder(conn, kind)?;

    // 学習
    let dict_blob = zstd::dict::from_samples(samples_for_training, capacity)?;
    let sample_bytes: usize = samples_for_training.iter().map(|s| s.len()).sum();
    let new_encoder = EncoderDictionary::copy(&dict_blob, crate::compress::LEVEL);

    // 既存行を旧 decoder（or 辞書なし）で読み出し、新 encoder で再圧縮
    let (table, blob_col) = table_for(kind)?;
    let recompress = recompress_all_with_new_dict(
        conn,
        table,
        blob_col,
        old_decoder.as_deref(),
        &new_encoder,
        batch,
        &mut on_progress,
    )?;

    // 最後に compression_dicts を UPSERT してキャッシュを invalidate
    let trained = upsert_trained_dict(
        conn,
        kind,
        &dict_blob,
        capacity,
        samples_for_training.len(),
        sample_bytes,
        now_ms,
    )?;

    Ok(RetrainStats {
        trained,
        recompress,
    })
}

fn table_for(kind: &str) -> anyhow::Result<(&'static str, &'static str)> {
    match kind {
        KIND_RAW => Ok(("messages", "raw_zstd")),
        KIND_PAYLOAD => Ok(("events", "payload_zstd")),
        other => anyhow::bail!("unknown kind: {other}"),
    }
}

/// 既存テーブル全行を「旧 decoder で読み取り、新 encoder で書き直す」汎用ループ。
/// `old_decoder=None` の場合は辞書なし zstd として読み取る（初回学習ケース）。
/// `text` 列は触らないので `messages_au` トリガ・FTS index は無関係。
fn recompress_all_with_new_dict(
    conn: &Connection,
    table: &str,
    blob_col: &str,
    old_decoder: Option<&DecoderDictionary<'static>>,
    new_encoder: &EncoderDictionary<'static>,
    batch: usize,
    on_progress: &mut impl FnMut(RecompressProgress),
) -> anyhow::Result<RecompressStats> {
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {blob_col} IS NOT NULL"),
        [],
        |r| r.get(0),
    )?;
    let mut stats = RecompressStats::default();
    if total == 0 {
        on_progress(RecompressProgress {
            processed: 0,
            total: 0,
            bytes_before: 0,
            bytes_after: 0,
        });
        return Ok(stats);
    }
    let batch = batch.max(1) as i64;
    let mut last_id: i64 = 0;

    let select_sql = format!(
        "SELECT id, {blob_col} FROM {table}
         WHERE {blob_col} IS NOT NULL AND id > ?1
         ORDER BY id ASC
         LIMIT ?2"
    );
    let update_sql = format!("UPDATE {table} SET {blob_col} = ?1 WHERE id = ?2");

    loop {
        let tx = conn.unchecked_transaction()?;
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = tx.prepare(&select_sql)?;
            let mapped = stmt
                .query_map(rusqlite::params![last_id, batch], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            mapped
        };
        if rows.is_empty() {
            break;
        }
        for (id, zblob) in &rows {
            last_id = *id;
            let text = match old_decoder {
                Some(dec) => crate::compress::decode_blob_to_string_with_dict(zblob, dec)?,
                None => crate::compress::decode_blob_to_string(zblob)?,
            };
            let before = zblob.len() as u64;
            let new_z = crate::compress::encode_str_with_dict(&text, new_encoder)?;
            let after = new_z.len() as u64;
            tx.execute(&update_sql, rusqlite::params![new_z, id])?;
            stats.processed += 1;
            stats.bytes_before += before;
            stats.bytes_after += after;
        }
        tx.commit()?;
        on_progress(RecompressProgress {
            processed: stats.processed,
            total: total as usize,
            bytes_before: stats.bytes_before,
            bytes_after: stats.bytes_after,
        });
    }
    Ok(stats)
}

/// 既存行のフィルタなしランダムサンプリングで学習用バイト列を集める。
/// 旧辞書あり/なし両ケースを透過的に decode する。
pub fn collect_training_samples(
    conn: &Connection,
    kind: &str,
    max_samples: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let (table, blob_col) = table_for(kind)?;
    let old_decoder = load_decoder(conn, kind)?;

    let sql = format!(
        "SELECT {blob_col} FROM {table}
         WHERE {blob_col} IS NOT NULL
         ORDER BY RANDOM() LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let blobs: Vec<Vec<u8>> = {
        let mapped = stmt
            .query_map([max_samples as i64], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        mapped
    };
    let mut out = Vec::with_capacity(blobs.len());
    for z in blobs {
        let text = match &old_decoder {
            Some(dec) => crate::compress::decode_blob_to_string_with_dict(&z, dec),
            None => crate::compress::decode_blob_to_string(&z),
        };
        if let Ok(t) = text {
            out.push(t.into_bytes());
        }
    }
    Ok(out)
}

/// 再圧縮の進捗（kind 単位）。
#[derive(Debug, Clone, Copy)]
pub struct RecompressProgress {
    pub processed: usize,
    pub total: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// 再圧縮の集計。
#[derive(Debug, Default, Clone, Copy)]
pub struct RecompressStats {
    pub processed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// `retrain_kind` 全体の結果。
#[derive(Debug, Clone, Copy)]
pub struct RetrainStats {
    pub trained: TrainedDict,
    pub recompress: RecompressStats,
}

/// 辞書一覧（管理表示用）。
#[derive(Debug, Clone)]
pub struct DictRow {
    pub kind: String,
    pub blob_len: i64,
    pub capacity: Option<i64>,
    pub sample_count: Option<i64>,
    pub sample_bytes: Option<i64>,
    pub created_at: i64,
}

pub fn list(conn: &Connection) -> anyhow::Result<Vec<DictRow>> {
    let mut stmt = conn.prepare(
        "SELECT kind, LENGTH(dict_blob), capacity, sample_count, sample_bytes, created_at
         FROM compression_dicts
         ORDER BY kind",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(DictRow {
                kind: r.get(0)?,
                blob_len: r.get(1)?,
                capacity: r.get(2)?,
                sample_count: r.get(3)?,
                sample_bytes: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_in_memory;

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn upsert_replaces_existing_kind() {
        let conn = open_in_memory().unwrap();
        upsert_trained_dict(&conn, KIND_RAW, &[1, 2, 3], 1024, 10, 100, 1).unwrap();
        upsert_trained_dict(&conn, KIND_RAW, &[4, 5, 6], 2048, 20, 200, 2).unwrap();

        // 同 kind は 1 行に上書きされる
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM compression_dicts"), 1);
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT dict_blob FROM compression_dicts WHERE kind = 'raw'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blob, vec![4, 5, 6]);
    }

    #[test]
    fn retrain_full_cycle_dict_only() {
        // 初回学習: 既存行は辞書なし zstd で書かれている → retrain で辞書あり版に変換される
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES ('s1')", [])
            .unwrap();

        // 辞書なしで 30 行を投入
        for i in 0..30 {
            let raw = format!(
                r#"{{"type":"assistant","sessionId":"s1","uuid":"u{i:02}","message":{{"content":[{{"type":"text","text":"hello{i}"}}]}}}}"#
            );
            let z = crate::compress::encode_str(&raw).unwrap();
            conn.execute(
                "INSERT INTO messages(session_id, uuid, seq, role, msg_type, text, raw_zstd)
                 VALUES ('s1', ?1, ?2, 'assistant', 'text', ?3, ?4)",
                rusqlite::params![format!("u{i:02}"), i as i64, format!("hello{i}"), z],
            )
            .unwrap();
        }

        // サンプルを集めて retrain
        let samples = collect_training_samples(&conn, KIND_RAW, 30).unwrap();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let stats = retrain_kind(&conn, KIND_RAW, &refs, 4096, 10, 1234, |_| {}).unwrap();
        assert_eq!(stats.recompress.processed, 30);

        // 全行を新辞書で decode できる（show_session 経由で）
        let msgs = crate::query::show_session(&conn, "s1").unwrap();
        assert_eq!(msgs.len(), 30);
        for (i, m) in msgs.iter().enumerate() {
            assert!(m.raw.as_deref().unwrap().contains(&format!("hello{i}")));
        }

        // compression_dicts は 1 行（kind='raw'）
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM compression_dicts"), 1);
    }

    #[test]
    fn retrain_replaces_old_dict_and_keeps_roundtrip() {
        // 旧辞書で圧縮されたデータを、新辞書に置き換えても roundtrip が成立する
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES ('s1')", [])
            .unwrap();
        for i in 0..20 {
            let raw = format!(r#"{{"sessionId":"s1","uuid":"u{i}","content":"hello{i}"}}"#);
            let z = crate::compress::encode_str(&raw).unwrap();
            conn.execute(
                "INSERT INTO messages(session_id, uuid, seq, role, msg_type, text, raw_zstd)
                 VALUES ('s1', ?1, ?2, 'assistant', 'text', NULL, ?3)",
                rusqlite::params![format!("u{i}"), i as i64, z],
            )
            .unwrap();
        }
        // 1 回目 retrain (旧辞書なし → 新辞書 A)
        let samples = collect_training_samples(&conn, KIND_RAW, 20).unwrap();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        retrain_kind(&conn, KIND_RAW, &refs, 4096, 10, 1, |_| {}).unwrap();

        // 2 回目 retrain (辞書 A → 辞書 B)
        let samples2 = collect_training_samples(&conn, KIND_RAW, 20).unwrap();
        let refs2: Vec<&[u8]> = samples2.iter().map(|v| v.as_slice()).collect();
        retrain_kind(&conn, KIND_RAW, &refs2, 4096, 10, 2, |_| {}).unwrap();

        // 中身が壊れていない
        let msgs = crate::query::show_session(&conn, "s1").unwrap();
        assert_eq!(msgs.len(), 20);
        for (i, m) in msgs.iter().enumerate() {
            assert!(m.raw.as_deref().unwrap().contains(&format!("hello{i}")));
        }
    }

    #[test]
    fn payload_and_raw_dicts_are_independent() {
        let conn = open_in_memory().unwrap();
        upsert_trained_dict(&conn, KIND_RAW, &[1; 16], 1024, 1, 1, 1).unwrap();
        upsert_trained_dict(&conn, KIND_PAYLOAD, &[2; 16], 1024, 1, 1, 2).unwrap();
        let raw_blob = load_blob(&conn, KIND_RAW).unwrap().unwrap();
        let pay_blob = load_blob(&conn, KIND_PAYLOAD).unwrap().unwrap();
        assert_ne!(raw_blob, pay_blob);
    }
}
