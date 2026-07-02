//! リモート（揮発性マシン）の transcript・メモリを rsync で pull し、取り込む。
//!
//! リモートの `~/.ccc/agent_settings/claude/<profile>/` を **ローカルのステージング**
//! `~/.ccc/archive/pulled/<host>/<profile>/` へ rsync pull し（ローカルと同一レイアウト）、
//! 既存の取り込み器（[`crate::ingest`]・[`crate::memory`]）に通す。pull したセッションは
//! `kind='remote'` を付け、帰属は hook 正本がなければ推定補完で `inferred`/`host` にする。
//!
//! rsync/ssh は ccc 本体の push 経路（`agent_config.rs`）と同条件（既存 ssh 設定・
//! ControlMaster を再利用）。`.credentials.json` は include パターンに含めないことで常に除外する。

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;

use crate::ingest::scan_projects;
use crate::memory::snapshot_memory;

/// pull＋取り込みの結果サマリ。
#[derive(Debug, Default, Clone, Copy)]
pub struct PullStats {
    /// 取り込んだ transcript ファイル数。
    pub files: usize,
    /// 新規挿入したメッセージ数。
    pub new_messages: usize,
    /// 帰属を反映したセッション数。
    pub sessions: usize,
    /// 新版として保存したメモリ数。
    pub memory_new: usize,
}

/// ステージングのプロファイルディレクトリ `<staging_root>/<host>/<profile>/` を返す。
pub fn staging_profile_dir(staging_root: &Path, host_alias: &str, profile: &str) -> PathBuf {
    staging_root.join(host_alias).join(profile)
}

/// リモート profile 配下の `projects/` とメモリ資産を **全面 sweep** で pull する。
///
/// 取りこぼし回収（切断・終了・再接続時）用。delta 転送なので 2 回目以降は安価。
pub fn pull_profile(host_alias: &str, profile: &str, staging_root: &Path) -> anyhow::Result<()> {
    let dest = staging_profile_dir(staging_root, host_alias, profile);
    std::fs::create_dir_all(&dest)?;
    // リモートは push と同じく home 相対（`host:.ccc/...`）。
    let src = format!("{host_alias}:.ccc/agent_settings/claude/{profile}/");
    // include/exclude 順序が肝: まず全ディレクトリへ降下を許可し、取り込み対象だけ
    // include、残りを除外する。`.credentials.json` は include されないため自然に除外されるが、
    // include パターンが将来緩んでも漏れないよう、先頭で明示除外しておく（多層防御）。
    let status = Command::new("rsync")
        .args([
            "-az",
            "--safe-links",
            "--prune-empty-dirs",
            "--exclude=.credentials.json",
            "--include=*/",
            "--include=projects/***",
            "--include=CLAUDE.md",
            "--include=memory/***",
            "--include=MEMORY.md",
            "--include=rules/***",
            "--exclude=*",
        ])
        .arg(&src)
        .arg(format!("{}/", dest.display()))
        .status();
    check_rsync(status, host_alias, "pull_profile")
}

/// hook payload の `transcript_path`（リモート絶対パス）を起点に、対象セッションの
/// `<sid>.jsonl` と兄弟ディレクトリ `<sid>/`（subagents/・tool-results/）だけを狙い撃ち pull する。
///
/// 全 `projects/` を舐めないため定期・境界トリガで安価。`transcript_path` から
/// `projects/<enc>/<sid>.jsonl` 部分を取り出し、rsync `-R`（相対）でステージングに復元する。
pub fn pull_session(
    host_alias: &str,
    profile: &str,
    transcript_path: &str,
    staging_root: &Path,
) -> anyhow::Result<()> {
    // `.../projects/<enc>/<sid>.jsonl` → `<enc>/<sid>.jsonl`
    let Some((_, after)) = transcript_path.rsplit_once("/projects/") else {
        anyhow::bail!("transcript_path に projects/ が含まれません: {transcript_path}");
    };
    let rel_file = format!("projects/{after}");
    let rel_dir = rel_file
        .strip_suffix(".jsonl")
        .map(|s| format!("{s}/"))
        .unwrap_or_default();

    let dest = staging_profile_dir(staging_root, host_alias, profile);
    std::fs::create_dir_all(&dest)?;
    let remote_profile = format!(".ccc/agent_settings/claude/{profile}");

    // 本体 jsonl（`/./` は rsync -R の相対ルート指定）。狙い撃ちでフィルタが無いため、
    // 兄弟ディレクトリ配下に混入し得る `.credentials.json` を明示除外する（多層防御）。
    let status = Command::new("rsync")
        .args(["-azR", "--safe-links", "--exclude=.credentials.json"])
        .arg(format!("{host_alias}:{remote_profile}/./{rel_file}"))
        .arg(format!("{}/", dest.display()))
        .status();
    check_rsync(status, host_alias, "pull_session(file)")?;

    // 兄弟ディレクトリ（無いことも多い）。存在しなければ rsync が非ゼロを返すが、
    // 致命的ではないため失敗は無視する。
    if !rel_dir.is_empty() {
        let _ = Command::new("rsync")
            .args(["-azR", "--safe-links", "--exclude=.credentials.json"])
            .arg(format!("{host_alias}:{remote_profile}/./{rel_dir}"))
            .arg(format!("{}/", dest.display()))
            .status();
    }
    Ok(())
}

/// ステージングされた profile ディレクトリを取り込み、`kind='remote'` と帰属を反映する。
/// メモリ資産も `source_kind='remote'` で保全する。冪等（再取り込みは無害）。
pub fn ingest_pulled(
    conn: &Connection,
    staging_profile_dir: &Path,
    host_alias: &str,
    profile: &str,
) -> anyhow::Result<PullStats> {
    let projects = staging_profile_dir.join("projects");
    let scan = scan_projects(conn, &projects)?;

    let sids = collect_session_ids(&projects);
    for sid in &sids {
        apply_remote_attribution(conn, sid, host_alias, profile)?;
    }

    let mem = snapshot_memory(
        conn,
        staging_profile_dir,
        profile,
        "remote",
        Some(host_alias),
    )?;

    Ok(PullStats {
        files: scan.files,
        new_messages: scan.new_messages,
        sessions: sids.len(),
        memory_new: mem.new_versions,
    })
}

/// pull したセッション 1 件に remote メタを付与し、帰属を確定する。
///
/// `kind`/`host_alias` は pull で確定する事実なので上書きする。`attribution='hook'`
/// （正本）は最優先で温存し、それ以外のときだけ §5 の推定補完を適用する。
fn apply_remote_attribution(
    conn: &Connection,
    session_id: &str,
    host_alias: &str,
    profile: &str,
) -> anyhow::Result<()> {
    let row = conn
        .query_row(
            "SELECT attribution, cwd, started_at FROM sessions WHERE session_id=?1",
            [session_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .ok();
    let Some((attribution, cwd, started_at)) = row else {
        return Ok(()); // ingest 直後なので通常は存在する。無ければ何もしない。
    };

    conn.execute(
        "UPDATE sessions
           SET kind='remote', host_alias=?2, agent_profile=COALESCE(agent_profile, ?3)
         WHERE session_id=?1",
        rusqlite::params![session_id, host_alias, profile],
    )?;

    if attribution.as_deref() == Some("hook") {
        return Ok(()); // 正本が勝つ。推定しない。
    }

    let (instance_id, instance_name, attr) =
        infer_attribution(conn, host_alias, profile, cwd.as_deref(), started_at)?;
    conn.execute(
        "UPDATE sessions
           SET attribution=?2,
               instance_id   = COALESCE(?3, instance_id),
               instance_name = COALESCE(?4, instance_name)
         WHERE session_id=?1 AND COALESCE(attribution,'') != 'hook'",
        rusqlite::params![session_id, attr, instance_id, instance_name],
    )?;
    Ok(())
}

/// §5 推定補完アルゴリズム。pull 元インスタンスを観測シグナルで推定する。
///
/// 戻り値 `(instance_id, instance_name, attribution)`。確信が持てなければ
/// `attribution='host'`（名前なし）に落とす（誤帰属より無名を選ぶ）。
pub fn infer_attribution(
    conn: &Connection,
    host_alias: &str,
    profile: &str,
    cwd: Option<&str>,
    started_at: Option<i64>,
) -> anyhow::Result<(Option<String>, Option<String>, String)> {
    // Step 1: host_alias ＋ agent_profile でハード絞り込み。
    let mut stmt = conn.prepare(
        "SELECT instance_id, name, directory FROM instances
         WHERE host_alias=?1 AND agent_profile=?2",
    )?;
    let candidates: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map(rusqlite::params![host_alias, profile], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    if candidates.is_empty() {
        return Ok((None, None, "host".into()));
    }

    // Step 2: cwd 一致（錨）。cwd 無し or 一致無しなら推定しない（別 cwd へは帰属させない）。
    let Some(cwd) = cwd else {
        return Ok((None, None, "host".into()));
    };
    let cwd_n = normalize_dir(cwd);
    let matched: Vec<&(String, Option<String>, Option<String>)> = candidates
        .iter()
        .filter(|(_, _, dir)| dir.as_deref().map(normalize_dir).as_deref() == Some(cwd_n.as_str()))
        .collect();
    if matched.is_empty() {
        return Ok((None, None, "host".into()));
    }
    if matched.len() == 1 {
        let (id, name, _) = matched[0];
        return Ok((Some(id.clone()), name.clone(), "inferred".into()));
    }

    // Step 3: 時間で曖昧性解消。各候補の観測活動窓 W_i=[min,max event.ts]。
    let Some(started) = started_at else {
        return Ok((None, None, "host".into())); // 時刻が無いと一意化できない。
    };
    // (instance_id, name, 活動窓 [min,max event.ts])
    type CandidateWindow<'a> = (&'a str, Option<String>, Option<(i64, i64)>);
    let windows: Vec<CandidateWindow> = matched
        .iter()
        .map(|(id, name, _)| (id.as_str(), name.clone(), activity_window(conn, id)))
        .collect();

    // (a) W_i が started を包含する候補を優先。(b) 複数なら最も狭い窓。
    let mut containing: Vec<&CandidateWindow> = windows
        .iter()
        .filter(|(_, _, w)| matches!(w, Some((lo, hi)) if *lo <= started && started <= *hi))
        .collect();
    if !containing.is_empty() {
        containing.sort_by_key(|(_, _, w)| w.map(|(lo, hi)| hi - lo).unwrap_or(i64::MAX));
        // 最狭窓が一意か（同幅タイは曖昧として host に落とす）。
        let narrowest = containing[0].2.map(|(lo, hi)| hi - lo);
        let ties = containing
            .iter()
            .filter(|(_, _, w)| w.map(|(lo, hi)| hi - lo) == narrowest)
            .count();
        if ties == 1 {
            let (id, name, _) = containing[0];
            return Ok((Some(id.to_string()), name.clone(), "inferred".into()));
        }
        return Ok((None, None, "host".into()));
    }

    // (c) 包含ゼロ → started に最も近い窓。ただしギャップ ≤ 閾値のときだけ。
    let mut best: Option<(&str, Option<String>, i64)> = None;
    for (id, name, w) in &windows {
        if let Some((lo, hi)) = w {
            let gap = if started < *lo {
                lo - started
            } else {
                started - hi
            };
            if best.as_ref().map(|b| gap < b.2).unwrap_or(true) {
                best = Some((id, name.clone(), gap));
            }
        }
    }
    match best {
        Some((id, name, gap)) if gap <= NEAR_THRESHOLD_MS => {
            Ok((Some(id.to_string()), name, "inferred".into()))
        }
        _ => Ok((None, None, "host".into())),
    }
}

/// 包含ゼロ時に「最も近い」と認める時間ギャップの上限（1 時間）。
const NEAR_THRESHOLD_MS: i64 = 3_600_000;

/// インスタンスの観測活動窓 `[min(event.ts), max(event.ts)]`。events が無ければ None。
fn activity_window(conn: &Connection, instance_id: &str) -> Option<(i64, i64)> {
    conn.query_row(
        "SELECT MIN(ts), MAX(ts) FROM events WHERE instance_id=?1 AND ts IS NOT NULL",
        [instance_id],
        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
    )
    .ok()
    .and_then(|(lo, hi)| Some((lo?, hi?)))
}

/// ディレクトリパスの正規化（末尾スラッシュ除去）。
fn normalize_dir(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// ステージングの `projects/*/` 直下にある `<sid>.jsonl` から session_id を集める。
fn collect_session_ids(projects_dir: &Path) -> Vec<String> {
    let mut sids = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        return sids;
    };
    for proj in projects.flatten() {
        let pdir = proj.path();
        if !pdir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&pdir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        sids.push(stem.to_string());
                    }
                }
            }
        }
    }
    sids
}

/// rsync の実行結果を判定し、分かりやすいエラーに整える。
fn check_rsync(
    status: std::io::Result<std::process::ExitStatus>,
    host_alias: &str,
    label: &str,
) -> anyhow::Result<()> {
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) if s.code() == Some(127) => anyhow::bail!(
            "リモート '{host_alias}' に rsync がありません（{label}）。リモートで rsync を導入してください。"
        ),
        Ok(s) => anyhow::bail!("rsync 失敗（{label}, host={host_alias}): {s}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("ローカルに rsync がありません（{label}）。`brew install rsync` 等で導入してください。")
        }
        Err(e) => anyhow::bail!("rsync 実行エラー（{label}）: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// instances 台帳と events を投入し、推定補完の各分岐を固定する。
    fn seed_instances(conn: &Connection) {
        // i1, i2 とも host=h, profile=p。i1 は cwd=/work/a、i2 は cwd=/work/b。
        for (id, dir) in [("i1", "/work/a"), ("i2", "/work/b")] {
            conn.execute(
                "INSERT INTO instances(instance_id, name, host_alias, agent_profile, directory)
                 VALUES (?1, ?1, 'h', 'p', ?2)",
                rusqlite::params![id, dir],
            )
            .unwrap();
        }
    }

    #[test]
    fn infer_no_candidates_is_host() {
        let conn = crate::open_in_memory().unwrap();
        let (id, name, attr) =
            infer_attribution(&conn, "h", "p", Some("/work/a"), Some(100)).unwrap();
        assert_eq!(attr, "host");
        assert!(id.is_none() && name.is_none());
    }

    #[test]
    fn infer_unique_cwd_is_inferred() {
        let conn = crate::open_in_memory().unwrap();
        seed_instances(&conn);
        let (id, name, attr) =
            infer_attribution(&conn, "h", "p", Some("/work/a/"), Some(100)).unwrap();
        assert_eq!(attr, "inferred");
        assert_eq!(id.as_deref(), Some("i1"));
        assert_eq!(name.as_deref(), Some("i1"));
    }

    #[test]
    fn infer_no_cwd_match_is_host() {
        let conn = crate::open_in_memory().unwrap();
        seed_instances(&conn);
        let (_, _, attr) =
            infer_attribution(&conn, "h", "p", Some("/work/zzz"), Some(100)).unwrap();
        assert_eq!(attr, "host");
    }

    #[test]
    fn infer_time_window_disambiguates_same_cwd() {
        let conn = crate::open_in_memory().unwrap();
        // 同一 cwd の 2 インスタンス。活動窓で曖昧性解消する。
        for id in ["i1", "i2"] {
            conn.execute(
                "INSERT INTO instances(instance_id, name, host_alias, agent_profile, directory)
                 VALUES (?1, ?1, 'h', 'p', '/work/a')",
                [id],
            )
            .unwrap();
        }
        // i1 の窓 [0,50]、i2 の窓 [100,200]。started=150 は i2 を包含。
        for (iid, ts) in [("i1", 0), ("i1", 50), ("i2", 100), ("i2", 200)] {
            conn.execute(
                "INSERT INTO events(instance_id, hook_event, ts) VALUES (?1, 'X', ?2)",
                rusqlite::params![iid, ts],
            )
            .unwrap();
        }
        let (id, _, attr) = infer_attribution(&conn, "h", "p", Some("/work/a"), Some(150)).unwrap();
        assert_eq!(attr, "inferred");
        assert_eq!(id.as_deref(), Some("i2"));

        // どちらの窓にも入らず、両方とも閾値超え → host。
        let (_, _, attr2) =
            infer_attribution(&conn, "h", "p", Some("/work/a"), Some(99_999_999)).unwrap();
        assert_eq!(attr2, "host");
    }

    #[test]
    fn ingest_pulled_marks_remote_and_attribution() {
        use std::io::Write;
        // ステージングを temp に作る。
        let root = std::env::temp_dir().join(format!("ccc-pull-{}", std::process::id()));
        let prof = root.join("h").join("p");
        let proj = prof.join("projects").join("-work-a");
        std::fs::create_dir_all(&proj).unwrap();
        let jsonl = proj.join("s9.jsonl");
        let mut f = std::fs::File::create(&jsonl).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"s9","uuid":"u1","timestamp":"2026-05-25T02:00:00.000Z","cwd":"/work/a","message":{{"role":"user","content":"リモートのタスク"}}}}"#
        )
        .unwrap();
        std::fs::write(prof.join("CLAUDE.md"), "# リモートメモリ").unwrap();

        let conn = crate::open_in_memory().unwrap();
        // 同一 host/profile/cwd のインスタンスを 1 つ置く → inferred になるはず。
        conn.execute(
            "INSERT INTO instances(instance_id, name, host_alias, agent_profile, directory)
             VALUES ('iX','my-remote','h','p','/work/a')",
            [],
        )
        .unwrap();

        let stats = ingest_pulled(&conn, &prof, "h", "p").unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.new_messages, 1);
        assert_eq!(stats.memory_new, 1);

        let (kind, host, attr, iname): (String, String, String, Option<String>) = conn
            .query_row(
                "SELECT kind, host_alias, attribution, instance_name FROM sessions WHERE session_id='s9'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "remote");
        assert_eq!(host, "h");
        assert_eq!(attr, "inferred");
        assert_eq!(iname.as_deref(), Some("my-remote"));

        // メモリも remote 由来で保全される。
        let src: String = conn
            .query_row(
                "SELECT source_kind FROM memory_snapshots WHERE rel_path='CLAUDE.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src, "remote");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hook_attribution_is_not_overwritten() {
        let conn = crate::open_in_memory().unwrap();
        // 既に hook で確定済みのセッション。
        conn.execute(
            "INSERT INTO sessions(session_id, attribution, instance_name, cwd, started_at)
             VALUES ('s1','hook','authoritative','/work/a', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO instances(instance_id, name, host_alias, agent_profile, directory)
             VALUES ('iX','other','h','p','/work/a')",
            [],
        )
        .unwrap();
        apply_remote_attribution(&conn, "s1", "h", "p").unwrap();
        let (attr, name): (String, String) = conn
            .query_row(
                "SELECT attribution, instance_name FROM sessions WHERE session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attr, "hook", "hook 正本は温存");
        assert_eq!(name, "authoritative");
    }
}
