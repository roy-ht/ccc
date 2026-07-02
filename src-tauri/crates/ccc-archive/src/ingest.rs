//! transcript JSONL の増分取り込み。
//!
//! transcript を真実の源とし、ファイル末尾の未読バイト（`ingest_cursors.last_offset`
//! 以降）だけを読む。行は `uuid` で冪等（`UNIQUE(session_id, uuid)`）。append 中の
//! race を避けるため、**最後の改行までの完全な行のみ**を処理し、オフセットはそこまで進める。
//!
//! 取り込む `messages` 行は `user`/`assistant`/`system` のみ。`cwd`/`aiTitle`/`timestamp`
//! は全行を見て `sessions` メタに反映する。subagent 会話は別ファイル
//! `<session-id>/subagents/agent-*.jsonl` にあり、同じ取込器に通す（`is_sidechain=1`）。
//!
//! `sessions.cwd` の意味は「**セッション起点 cwd**」（＝起動ディレクトリ）。transcript の
//! 行ごとの cwd はセッション中の cd に追従するため、最初に観測した値だけを採用し以降は
//! 上書きしない（インスタンス＝host＋directory への紐付けアンカーとして使われる）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;

/// ディレクトリ走査の取り込み結果。
#[derive(Debug, Default, Clone, Copy)]
pub struct IngestStats {
    pub files: usize,
    pub new_messages: usize,
}

/// transcript（本体 jsonl）と、その兄弟 `<sid>/subagents/*.jsonl` を取り込む。
pub fn ingest_transcript(conn: &Connection, main_jsonl: &Path) -> anyhow::Result<usize> {
    let mut new = ingest_file(conn, main_jsonl)?;
    // 兄弟 subagents ディレクトリ: <親dir>/<stem>/subagents/agent-*.jsonl
    if let Some(stem) = main_jsonl.file_stem().and_then(|s| s.to_str()) {
        let sub = main_jsonl.with_file_name(stem).join("subagents");
        if sub.is_dir() {
            for entry in std::fs::read_dir(&sub)?.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    new += ingest_file(conn, &p)?;
                }
            }
        }
    }
    Ok(new)
}

/// `projects/<encoded-cwd>/<session-id>.jsonl` 群をまとめて取り込む（起動時フルスキャン用）。
pub fn scan_projects(conn: &Connection, projects_dir: &Path) -> anyhow::Result<IngestStats> {
    let mut stats = IngestStats::default();
    if !projects_dir.is_dir() {
        return Ok(stats);
    }
    for proj in std::fs::read_dir(projects_dir)?.flatten() {
        let pdir = proj.path();
        if !pdir.is_dir() {
            continue;
        }
        // 1 プロジェクトディレクトリの読込失敗で全スキャンを止めない。
        let entries = match std::fs::read_dir(&pdir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "[ccc-archive] プロジェクト読込スキップ ({}): {e}",
                    pdir.display()
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                stats.files += 1;
                // 1 ファイルの取込失敗（I/O・破損・一時的 DB ロック等）でも継続する。
                // カーソルは前進しないため、次回スキャンで冪等に再取込される。
                match ingest_transcript(conn, &p) {
                    Ok(n) => stats.new_messages += n,
                    Err(e) => eprintln!("[ccc-archive] 取込スキップ ({}): {e}", p.display()),
                }
                // 取り込み済みファイルはカーソルが先頭行を再読しないため、
                // 起点 cwd のズレ（過去の上書き仕様の残骸）はここで修復する。
                if let Err(e) = repair_session_start_cwd(conn, &p) {
                    eprintln!("[ccc-archive] cwd 修復スキップ ({}): {e}", p.display());
                }
                // 同様に、旧仕様（COALESCE のみ）で aiTitle が破棄されて first_user
                // のまま固まった summary をここで修復する。
                if let Err(e) = repair_session_ai_title(conn, &p) {
                    eprintln!("[ccc-archive] aiTitle 修復スキップ ({}): {e}", p.display());
                }
            }
        }
    }
    Ok(stats)
}

/// transcript 先頭部から「セッション起点 cwd」を読み取り、`sessions.cwd` を起点値に揃える。
///
/// 旧仕様の増分取り込みは「最後に取り込んだバッチの先頭 cwd」で cwd を毎回上書きして
/// いたため、セッション中に cd するとプロジェクト外のサブディレクトリに化けた行が残る。
/// 増分カーソルは先頭行を再読しないので、スキャン経路（起動時フルスキャン・pull 取込）
/// から本関数で修復する。冪等（既に起点値なら no-op）。修復した場合 `true` を返す。
pub fn repair_session_start_cwd(conn: &Connection, main_jsonl: &Path) -> anyhow::Result<bool> {
    let Some((session_id, cwd)) = read_start_cwd(main_jsonl)? else {
        return Ok(false);
    };
    let project = derive_project(&cwd);
    let changed = conn.execute(
        "UPDATE sessions SET cwd=?2, project=?3
         WHERE session_id=?1 AND (cwd IS NULL OR cwd<>?2)",
        rusqlite::params![session_id, cwd, project],
    )?;
    if changed > 0 {
        eprintln!("[ccc-archive] 起点 cwd を修復: {session_id} → {cwd}");
    }
    Ok(changed > 0)
}

/// transcript から `aiTitle` 行を探し、`sessions.summary` を最新の aiTitle に揃える。
///
/// 旧仕様（`summary = COALESCE(summary, ?)`）では、先に first_user が書かれた後に
/// aiTitle 行が届いた場合、aiTitle 側の値が破棄されて first_user のまま固まる不具合が
/// あった。増分カーソルは既に前進しているため次回インジェストで自動修復されず、
/// ここでスキャン経路から一度だけ全行を読み直して修復する。冪等（既に aiTitle 値なら no-op）。
/// 修復した場合 `true` を返す。
pub fn repair_session_ai_title(conn: &Connection, main_jsonl: &Path) -> anyhow::Result<bool> {
    let Some((session_id, ai_title)) = read_last_ai_title(main_jsonl)? else {
        return Ok(false);
    };
    // 既に同値なら no-op。異なる or NULL のときだけ書き換える。
    let changed = conn.execute(
        "UPDATE sessions SET summary=?2
         WHERE session_id=?1 AND (summary IS NULL OR summary<>?2)",
        rusqlite::params![session_id, ai_title],
    )?;
    if changed > 0 {
        eprintln!("[ccc-archive] summary を aiTitle に修復: {session_id}");
    }
    Ok(changed > 0)
}

/// transcript 全体を 1 行ずつ読み、最後に現れた `aiTitle` と sessionId を返す。
///
/// aiTitle は Claude Code のあるバージョンでは何度か再生成されうる。最新の値を採用する。
fn read_last_ai_title(main_jsonl: &Path) -> anyhow::Result<Option<(String, String)>> {
    let file = std::fs::File::open(main_jsonl)?;
    let reader = BufReader::new(file);
    let stem = main_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .map(String::from);
    let mut last: Option<(String, String)> = None;
    for line in reader.lines() {
        let Ok(raw) = line else { continue };
        let raw = raw.trim();
        // 高速パス: `aiTitle` を含まない行は JSON parse をスキップ。
        if raw.is_empty() || !raw.contains("aiTitle") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(title) = v.get("aiTitle").and_then(Value::as_str) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        let sid = v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| stem.clone());
        if let Some(sid) = sid {
            last = Some((sid, truncate(title, 200)));
        }
    }
    Ok(last)
}

/// transcript 先頭部（最大 64KB）を読み、最初に現れた `cwd` とその行の
/// `sessionId`（無ければファイル名 stem）を返す。
fn read_start_cwd(main_jsonl: &Path) -> anyhow::Result<Option<(String, String)>> {
    const HEAD_LIMIT: usize = 64 * 1024;
    let mut file = std::fs::File::open(main_jsonl)?;
    let mut buf = vec![0u8; HEAD_LIMIT];
    let mut filled = 0usize;
    while filled < HEAD_LIMIT {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);

    let stem = main_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .map(String::from);
    for line in buf.split(|&b| b == b'\n') {
        let Ok(raw) = std::str::from_utf8(line) else {
            continue; // 末尾の途中切れ行は UTF-8 境界で壊れ得る。先頭探索には不要。
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            let sid = v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from)
                .or(stem);
            return Ok(sid.map(|s| (s, cwd.to_string())));
        }
    }
    Ok(None)
}

/// 1 ファイルを増分取り込みし、新規挿入したメッセージ数を返す。
pub fn ingest_file(conn: &Connection, path: &Path) -> anyhow::Result<usize> {
    let path_key = path.to_string_lossy().to_string();
    let len = std::fs::metadata(path)?.len();

    // カーソル取得（無ければ 0）。ファイル縮小はローテとみなし先頭から再読込。
    let (mut offset, last_size): (i64, i64) = conn
        .query_row(
            "SELECT last_offset, last_size FROM ingest_cursors WHERE file_path=?1",
            [&path_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    if (len as i64) < last_size {
        offset = 0;
    }
    if offset as u64 >= len {
        return Ok(0); // 新規バイト無し
    }

    // 未読部分を読み、最後の改行までを完全行とする。
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let process_until = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => 0, // 完全な行が無い（partial）→ 次回に持ち越し
    };
    let new_offset = offset + process_until as i64;

    // アクティブな共有辞書（schema v4: 1 DB に 1 辞書）。なければ辞書なしモードで書き続ける。
    let active_raw_dict = crate::dicts::load_encoder(conn, crate::dicts::KIND_RAW)?;

    let tx = conn.unchecked_transaction()?;
    let mut new_count = 0usize;
    let mut seq_counter: HashMap<String, i64> = HashMap::new();
    let mut metas: HashMap<String, SessMeta> = HashMap::new();

    for line in buf[..process_until].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let raw = match std::str::from_utf8(line) {
            Ok(s) => s.trim(),
            Err(_) => continue,
        };
        if raw.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue, // 壊れた行は skip
        };

        let Some(session_id) = v.get("sessionId").and_then(Value::as_str) else {
            continue;
        };

        // セッションメタ（cwd / aiTitle / 先頭ユーザープロンプト）を蓄積。
        let meta = metas.entry(session_id.to_string()).or_default();
        if meta.cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                meta.cwd = Some(c.to_string());
            }
        }
        if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
            meta.ai_title = Some(truncate(t, 200));
        }

        // メッセージ行の取り込み。
        if let Some(msg) = parse_message(&v, raw) {
            if meta.first_user.is_none() && msg.role == "user" {
                if let Some(t) = &msg.text {
                    let t = t.trim();
                    if !t.is_empty() && !is_noise_prompt(t) {
                        meta.first_user = Some(truncate(t, 200));
                    }
                }
            }
            let next = seq_counter
                .entry(session_id.to_string())
                .or_insert_with(|| {
                    tx.query_row(
                        "SELECT COUNT(*) FROM messages WHERE session_id=?1",
                        [session_id],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                });
            // schema v2 以降: `raw` には書かず、zstd 圧縮した BLOB を `raw_zstd` に積む。
            // schema v4 以降: 共有辞書があればそれで圧縮（1 DB に 1 辞書 = dict_id 列なし）。
            //
            // 保存前に空 thinking / redacted_thinking ブロックを除去する（暗号化のみで本文を
            // 持たない signature payload を圧縮対象から外す）。本文がある thinking は残るので
            // open model 由来の平文は保持される。詳細は `crate::strip`。
            let raw_filtered = crate::strip::strip_empty_thinking_blocks(raw);
            let raw_zstd = match &active_raw_dict {
                Some(enc) => crate::compress::encode_str_with_dict(&raw_filtered, enc)?,
                None => crate::compress::encode_str(&raw_filtered)?,
            };
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO messages
                   (session_id, uuid, parent_uuid, seq, ts, role, msg_type, tool_name,
                    is_sidechain, agent_id, text, raw_zstd)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                rusqlite::params![
                    session_id,
                    msg.uuid,
                    msg.parent_uuid,
                    *next,
                    msg.ts,
                    msg.role,
                    msg.msg_type,
                    msg.tool_name,
                    msg.is_sidechain as i64,
                    msg.agent_id,
                    msg.text,
                    raw_zstd,
                ],
            )?;
            if inserted > 0 {
                *next += 1;
                new_count += 1;
            }
        }
    }

    // セッション行を upsert し、集計をメッセージから再計算（再取込でも一貫）。
    // cwd/project は「セッション起点 cwd」（最初に観測した値）を保持する。
    // transcript の各行の cwd はセッション中の cd に追従して変わるため、
    // 後続バッチで上書きするとプロジェクト外のサブディレクトリに化けて
    // `sessions_for_location` のインスタンス紐付けから外れてしまう。
    for (session_id, meta) in &metas {
        tx.execute(
            "INSERT OR IGNORE INTO sessions(session_id) VALUES (?1)",
            [session_id],
        )?;
        let project = meta.cwd.as_deref().and_then(derive_project);
        // summary は aiTitle 優先。aiTitle は Claude Code が生成に数分〜数時間かかるため、
        // 先に first_user プロンプトが DB に入っている状態で後続バッチに aiTitle が届く
        // ケースがある。単純な COALESCE では既存の first_user が残って aiTitle が
        // 破棄されてしまうので、本バッチで aiTitle を観測したら強制的に上書きする。
        let ai_title_seen = meta.ai_title.is_some() as i64;
        let summary = meta.ai_title.clone().or_else(|| meta.first_user.clone());
        tx.execute(
            "UPDATE sessions SET
               started_at    = (SELECT MIN(ts) FROM messages WHERE session_id=?1),
               ended_at      = (SELECT MAX(ts) FROM messages WHERE session_id=?1),
               message_count = (SELECT COUNT(*) FROM messages WHERE session_id=?1),
               cwd           = COALESCE(cwd, ?2),
               project       = COALESCE(project, ?3),
               summary       = CASE WHEN ?5 = 1 THEN ?4 ELSE COALESCE(summary, ?4) END
             WHERE session_id=?1",
            rusqlite::params![session_id, meta.cwd, project, summary, ai_title_seen],
        )?;
    }

    // カーソル前進。
    tx.execute(
        "INSERT INTO ingest_cursors(file_path, last_offset, last_size, updated_at)
           VALUES (?1, ?2, ?3, strftime('%s','now')*1000)
         ON CONFLICT(file_path) DO UPDATE SET
           last_offset=excluded.last_offset,
           last_size=excluded.last_size,
           updated_at=excluded.updated_at",
        rusqlite::params![path_key, new_offset, len as i64],
    )?;

    tx.commit()?;
    Ok(new_count)
}

#[derive(Default)]
struct SessMeta {
    cwd: Option<String>,
    ai_title: Option<String>,
    first_user: Option<String>,
}

struct Msg {
    uuid: Option<String>,
    parent_uuid: Option<String>,
    ts: Option<i64>,
    role: String,
    msg_type: String,
    tool_name: Option<String>,
    is_sidechain: bool,
    agent_id: Option<String>,
    text: Option<String>,
}

/// `user`/`assistant`/`system` 行を `Msg` に変換。それ以外は None。
fn parse_message(v: &Value, _raw: &str) -> Option<Msg> {
    let ty = v.get("type")?.as_str()?;
    let role = match ty {
        "user" | "assistant" | "system" => ty,
        _ => return None,
    };
    let message = v.get("message");
    let (text, msg_type, tool_name) = match role {
        "assistant" => extract_assistant(message),
        "user" => {
            let (t, mt) = extract_user(message);
            (t, mt, None)
        }
        // system 行の本文は **トップレベル `content`**（`message` 配下ではない）。
        // away_summary や API エラー等の実本文だけを採る。`stop_hook_summary` /
        // `turn_duration` は content を持たない純メタなので text は None のまま
        // （UI は「System行を表示」トグルで raw から整形表示する）。`local_command`
        // の stdout ラッパー等のノイズタグは除外する。
        _ => (extract_system(v), "system".to_string(), None),
    };
    Some(Msg {
        uuid: v.get("uuid").and_then(Value::as_str).map(String::from),
        parent_uuid: v
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(String::from),
        ts: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso8601_to_unix_ms),
        role: role.to_string(),
        msg_type,
        tool_name,
        is_sidechain: v
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        agent_id: v.get("agentId").and_then(Value::as_str).map(String::from),
        text,
    })
}

/// assistant の `message.content[]` から text ブロックを結合。tool_use 名も拾う。
fn extract_assistant(message: Option<&Value>) -> (Option<String>, String, Option<String>) {
    let Some(content) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        // content が文字列のケース
        let t = message.and_then(value_to_text);
        return (t, "text".to_string(), None);
    };
    let mut texts = Vec::new();
    let mut tool_name = None;
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        texts.push(t.trim().to_string());
                    }
                }
            }
            Some("tool_use") => {
                if tool_name.is_none() {
                    tool_name = block.get("name").and_then(Value::as_str).map(String::from);
                }
            }
            _ => {}
        }
    }
    let msg_type = if !texts.is_empty() {
        "text"
    } else if tool_name.is_some() {
        "tool_use"
    } else {
        "thinking"
    };
    let text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    };
    (text, msg_type.to_string(), tool_name)
}

/// user の `message.content` を text に。文字列ならそのまま、配列なら tool_result を抽出。
fn extract_user(message: Option<&Value>) -> (Option<String>, String) {
    let Some(content) = message.and_then(|m| m.get("content")) else {
        return (None, "text".to_string());
    };
    if let Some(s) = content.as_str() {
        return (Some(s.to_string()), "text".to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut texts = Vec::new();
        for block in arr {
            if let Some(t) = block_text(block) {
                texts.push(t);
            }
        }
        let text = if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        };
        return (text, "tool_result".to_string());
    }
    (None, "text".to_string())
}

/// tool_result ブロックの content（文字列 or [{type:text,text}]）からテキストを取る。
fn block_text(block: &Value) -> Option<String> {
    let c = block.get("content")?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = c.as_array() {
        let joined: Vec<String> = arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(String::from))
            .collect();
        if !joined.is_empty() {
            return Some(joined.join("\n"));
        }
    }
    None
}

/// system 行のトップレベル `content`（文字列）から本文を取り出す。
/// 空白のみ／ハーネス由来ラッパータグ（`<local-command-stdout>` 等）は None にする。
fn extract_system(v: &Value) -> Option<String> {
    v.get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !is_noise_prompt(s))
        .map(String::from)
}

/// `message` が文字列 / `{content: "..."}` のいずれでもテキストを取り出す。
fn value_to_text(message: &Value) -> Option<String> {
    if let Some(s) = message.as_str() {
        return Some(s.to_string());
    }
    message
        .get("content")
        .and_then(Value::as_str)
        .map(String::from)
}

/// summary 候補に使うべきでないハーネス由来ラッパー行か判定する。
/// （`!` コマンドの caveat、スラッシュコマンドの定義、system-reminder 等）
fn is_noise_prompt(text: &str) -> bool {
    const TAGS: &[&str] = &[
        "<local-command-caveat>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "<system-reminder>",
    ];
    let t = text.trim_start();
    TAGS.iter().any(|tag| t.starts_with(tag))
}

/// cwd の末尾コンポーネントを表示用 project 名にする。
pub(crate) fn derive_project(cwd: &str) -> Option<String> {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `YYYY-MM-DDTHH:MM:SS(.sss)?Z` を unix ms に変換（UTC 固定）。
fn iso8601_to_unix_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;

    let time = rest.trim_end_matches('Z');
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time, ""),
    };
    let mut tp = hms.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let se: i64 = tp.next().unwrap_or("0").parse().ok()?;
    // 小数部は先頭3桁をミリ秒に（不足はゼロ埋め）。
    let mut ms_str: String = frac
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(3)
        .collect();
    while ms_str.len() < 3 {
        ms_str.push('0');
    }
    let ms: i64 = ms_str.parse().unwrap_or(0);

    let days = days_from_civil(y, mo, d);
    Some((((days * 24 + h) * 60 + mi) * 60 + se) * 1000 + ms)
}

/// Howard Hinnant の days_from_civil（1970-01-01 からの日数）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_jsonl(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-ingest-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}.jsonl"));
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn iso8601_known_value() {
        // 2026-05-25T02:08:56.883Z の unix ms を Python 等と突合した値。
        assert_eq!(
            iso8601_to_unix_ms("2026-05-25T02:08:56.883Z"),
            Some(1779674936883)
        );
        assert_eq!(iso8601_to_unix_ms("1970-01-01T00:00:00.000Z"), Some(0));
    }

    #[test]
    fn ingest_basic_and_fts() {
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s1",
            &[
                r#"{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","cwd":"/Users/h/mydocs/ccc","message":{"role":"user","content":"設計をレビューして"}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"a1","parentUuid":"u1","timestamp":"2026-05-25T02:00:05.000Z","message":{"content":[{"type":"thinking","thinking":"..."},{"type":"text","text":"会議の結論をまとめます"},{"type":"tool_use","name":"Write","input":{}}]}}"#,
                r#"{"type":"ai-title","sessionId":"s1","aiTitle":"設計レビュー"}"#,
            ],
        );
        let new = ingest_file(&conn, &path).unwrap();
        assert_eq!(new, 2, "user+assistant の2行");

        // FTS: 2字熟語が両行から引ける
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                ["会議"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // セッションメタ
        let (cwd, project, summary, count): (String, String, String, i64) = conn
            .query_row(
                "SELECT cwd, project, summary, message_count FROM sessions WHERE session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(cwd, "/Users/h/mydocs/ccc");
        assert_eq!(project, "ccc");
        assert_eq!(summary, "設計レビュー"); // ai-title 優先
        assert_eq!(count, 2);

        // assistant 行の tool_name / msg_type
        let (mt, tool): (String, String) = conn
            .query_row(
                "SELECT msg_type, tool_name FROM messages WHERE uuid='a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mt, "text");
        assert_eq!(tool, "Write");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn summary_skips_noise_prompts() {
        // 先頭が `!` コマンドの caveat → summary は次の本物プロンプトを採用。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s4",
            &[
                r#"{"type":"user","sessionId":"s4","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","message":{"role":"user","content":"<local-command-caveat>Caveat: ...</local-command-caveat>"}}"#,
                r#"{"type":"user","sessionId":"s4","uuid":"u2","timestamp":"2026-05-25T02:00:01.000Z","message":{"role":"user","content":"設計をレビューして"}}"#,
            ],
        );
        ingest_file(&conn, &path).unwrap();
        let summary: Option<String> = conn
            .query_row(
                "SELECT summary FROM sessions WHERE session_id='s4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summary.as_deref(), Some("設計をレビューして"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn system_content_extracted_but_pure_meta_has_no_text() {
        // away_summary（実本文）/ stop_hook_summary（純メタ）/ local_command（ノイズ）。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s5",
            &[
                r#"{"type":"system","subtype":"away_summary","sessionId":"s5","uuid":"y1","timestamp":"2026-05-25T02:00:00.000Z","content":"緑背景が出ない問題を修正中"}"#,
                r#"{"type":"system","subtype":"stop_hook_summary","sessionId":"s5","uuid":"y2","timestamp":"2026-05-25T02:00:01.000Z","hookCount":2}"#,
                r#"{"type":"system","subtype":"local_command","sessionId":"s5","uuid":"y3","timestamp":"2026-05-25T02:00:02.000Z","content":"<local-command-stdout></local-command-stdout>"}"#,
            ],
        );
        assert_eq!(ingest_file(&conn, &path).unwrap(), 3, "system 3 行");

        let text = |uuid: &str| -> Option<String> {
            conn.query_row("SELECT text FROM messages WHERE uuid=?1", [uuid], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(text("y1").as_deref(), Some("緑背景が出ない問題を修正中"));
        assert_eq!(text("y2"), None, "stop_hook_summary は純メタ");
        assert_eq!(text("y3"), None, "local_command の stdout ラッパーはノイズ");

        // 実本文を持つ system 行は FTS から引ける。
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                ["緑背景"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ingest_is_incremental_and_idempotent() {
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s2",
            &[
                r#"{"type":"user","sessionId":"s2","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","message":{"role":"user","content":"hello"}}"#,
            ],
        );
        assert_eq!(ingest_file(&conn, &path).unwrap(), 1);
        // 再取込: 新規バイト無し → 0
        assert_eq!(ingest_file(&conn, &path).unwrap(), 0);

        // 追記 → 増分のみ取込
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                f,
                r#"{{"type":"assistant","sessionId":"s2","uuid":"a1","timestamp":"2026-05-25T02:00:05.000Z","message":{{"content":[{{"type":"text","text":"hi"}}]}}}}"#
            )
            .unwrap();
        }
        assert_eq!(ingest_file(&conn, &path).unwrap(), 1);
        let total: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE session_id='s2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 2);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cwd_keeps_session_start_value_across_batches() {
        // セッション中に cd しても sessions.cwd は起点値のまま（後続バッチで上書きしない）。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s6",
            &[
                r#"{"type":"user","sessionId":"s6","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","cwd":"/work/ccc","message":{"role":"user","content":"開始"}}"#,
            ],
        );
        ingest_file(&conn, &path).unwrap();

        // cd 後の行を追記して 2 回目の増分取り込み。
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                f,
                r#"{{"type":"user","sessionId":"s6","uuid":"u2","timestamp":"2026-05-25T02:10:00.000Z","cwd":"/work/ccc/src-tauri","message":{{"role":"user","content":"サブディレクトリで作業"}}}}"#
            )
            .unwrap();
        }
        ingest_file(&conn, &path).unwrap();

        let (cwd, project): (String, String) = conn
            .query_row(
                "SELECT cwd, project FROM sessions WHERE session_id='s6'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cwd, "/work/ccc", "起点 cwd を維持");
        assert_eq!(project, "ccc");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn repair_fixes_overwritten_cwd() {
        // 旧仕様で上書きされた cwd を transcript 先頭行から修復する。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s7",
            &[
                r#"{"type":"user","sessionId":"s7","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","cwd":"/work/ccc","message":{"role":"user","content":"開始"}}"#,
            ],
        );
        ingest_file(&conn, &path).unwrap();
        // 旧仕様の上書き結果を再現。
        conn.execute(
            "UPDATE sessions SET cwd='/work/ccc/src-tauri', project='src-tauri' WHERE session_id='s7'",
            [],
        )
        .unwrap();

        assert!(repair_session_start_cwd(&conn, &path).unwrap());
        let (cwd, project): (String, String) = conn
            .query_row(
                "SELECT cwd, project FROM sessions WHERE session_id='s7'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cwd, "/work/ccc");
        assert_eq!(project, "ccc");

        // 冪等: 既に起点値なら no-op。
        assert!(!repair_session_start_cwd(&conn, &path).unwrap());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn partial_last_line_is_held_until_complete() {
        // 改行で終わらない行は次回まで持ち越す（append 中の race 対策）。
        let dir = std::env::temp_dir().join(format!("ccc-ingest-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s3.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            // 改行なしの不完全行
            write!(
                f,
                r#"{{"type":"user","sessionId":"s3","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","message":{{"content":"x"}}}}"#
            )
            .unwrap();
        }
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(ingest_file(&conn, &path).unwrap(), 0, "不完全行は持ち越し");
        // 改行を足して完成させる
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f).unwrap();
        }
        assert_eq!(ingest_file(&conn, &path).unwrap(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ai_title_in_later_batch_overwrites_first_user_summary() {
        // 実運用パターン: 最初のバッチには user プロンプトだけ、
        // aiTitle 行は数分〜数時間後に届く。バッチ間 aiTitle が既存 summary を
        // 上書きすることを確認する（旧仕様の COALESCE では失敗する）。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s-late-title",
            &[
                r#"{"type":"user","sessionId":"s-late-title","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","message":{"role":"user","content":"設計をレビューして"}}"#,
            ],
        );
        ingest_file(&conn, &path).unwrap();
        let summary: Option<String> = conn
            .query_row(
                "SELECT summary FROM sessions WHERE session_id='s-late-title'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            summary.as_deref(),
            Some("設計をレビューして"),
            "初回は first_user が入る"
        );

        // 後続バッチで aiTitle 行が届く。
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                f,
                r#"{{"type":"ai-title","sessionId":"s-late-title","aiTitle":"設計レビュー"}}"#
            )
            .unwrap();
        }
        ingest_file(&conn, &path).unwrap();

        let summary: Option<String> = conn
            .query_row(
                "SELECT summary FROM sessions WHERE session_id='s-late-title'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            summary.as_deref(),
            Some("設計レビュー"),
            "後続バッチの aiTitle が上書きする"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn repair_backfills_ai_title_from_transcript() {
        // 旧仕様で summary が first_user のまま固まったレコードを、
        // scan 経路の repair_session_ai_title で aiTitle に修復する。
        let conn = crate::open_in_memory().unwrap();
        let path = tmp_jsonl(
            "s-stuck",
            &[
                r#"{"type":"user","sessionId":"s-stuck","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","message":{"role":"user","content":"設計をレビューして"}}"#,
                r#"{"type":"ai-title","sessionId":"s-stuck","aiTitle":"設計レビュー"}"#,
            ],
        );
        ingest_file(&conn, &path).unwrap();
        // 旧仕様の詰まった状態を再現。
        conn.execute(
            "UPDATE sessions SET summary='設計をレビューして' WHERE session_id='s-stuck'",
            [],
        )
        .unwrap();

        assert!(repair_session_ai_title(&conn, &path).unwrap());
        let summary: Option<String> = conn
            .query_row(
                "SELECT summary FROM sessions WHERE session_id='s-stuck'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summary.as_deref(), Some("設計レビュー"));

        // 冪等: 既に aiTitle 値なら no-op。
        assert!(!repair_session_ai_title(&conn, &path).unwrap());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
