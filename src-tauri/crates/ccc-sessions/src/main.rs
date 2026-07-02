//! ccc-sessions: セッションアーカイブの取込・検索・閲覧 CLI。
//!
//! `ccc-archive` lib を共有し、ccc 本体と同じ DB（`~/.ccc[/dev]/archive/sessions.db`）を開く。
//! 既定は人間向け整形、`--json` で機械可読出力。

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ccc-sessions",
    about = "ccc セッションアーカイブの検索・閲覧・取込"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// transcript とメモリを取り込む。既定はローカル全プロファイル。
    /// `--host <alias>` でリモートを rsync pull して取り込む。
    Sync {
        /// 明示用フラグ（ローカル取込）
        #[arg(long)]
        local: bool,
        /// リモートホスト（ssh エイリアス）から pull して取り込む
        #[arg(long)]
        host: Option<String>,
        /// 対象プロファイル（--host 時に使用。既定 default）
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// 全文検索(日本語は形態素・BM25 順)。
    Search {
        query: String,
        #[arg(long)]
        project: Option<String>,
        /// 直近 N 日に絞る
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 30)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// セッション一覧（新しい順）。
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// 1 セッションのメッセージを表示（既定は要約。全文は --full）。
    Show {
        session_id: String,
        /// 本文を省略せず全文表示
        #[arg(long)]
        full: bool,
        /// 元の JSONL をそのまま表示
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        json: bool,
    },
    /// 直近 N 日の活動サマリ（振り返り用。日付ごとにまとめる）。
    Recent {
        #[arg(long, default_value_t = 1)]
        days: i64,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// アーカイブ全体の統計（件数・期間・内訳）。
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// メモリ資産（CLAUDE.md / memory / rules）の版管理。
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// 共有 zstd 辞書を学習・保存し、既存データを atomic に再圧縮する（schema v4 / §9）。
    /// **1 DB に 1 辞書** が不変条件なので再学習は常に全行再圧縮を伴う。
    /// 実行中は CCC 本体を停止すること。
    TrainDict {
        /// 対象: 'raw' (messages) / 'payload' (events) / 'both' (既定)
        #[arg(default_value = "both")]
        kind: String,
        /// 辞書サイズ上限 (KB)。zstd 推奨は 110KB
        #[arg(long, default_value_t = 110)]
        capacity: usize,
        /// 学習サンプル数
        #[arg(long, default_value_t = 5000)]
        samples: usize,
        /// VACUUM を skip
        #[arg(long)]
        no_vacuum: bool,
        /// 再圧縮の COMMIT バッチ
        #[arg(long, default_value_t = 1000)]
        batch: usize,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// 最新版の一覧。
    List {
        #[arg(long)]
        profile: Option<String>,
        /// 'user' | 'project'
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 内容を表示（既定は最新版。--version で content-hash 前方一致指定）。
    Show {
        rel_path: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 直近 2 版の行差分。
    Diff {
        rel_path: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// 過去版を CLAUDE_CONFIG_DIR に書き戻す（上書き前に現状を自動スナップショット）。
    Restore {
        rel_path: String,
        #[arg(long)]
        profile: Option<String>,
        /// 書き戻す版（content-hash 前方一致）。省略時は最新版。
        #[arg(long)]
        version: Option<String>,
        /// 確認プロンプトを省略して実行
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let db = db_path()?;
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn =
        ccc_archive::open(&db).with_context(|| format!("DB を開けません: {}", db.display()))?;

    match cli.cmd {
        Cmd::Sync {
            host: Some(host),
            profile,
            ..
        } => {
            // リモート: 全面 sweep pull → ステージング → 取り込み（kind=remote・帰属推定）。
            let staging = staging_root()?;
            eprintln!("rsync pull 中: {host}:{profile} …");
            ccc_archive::pull_profile(&host, &profile, &staging)?;
            let dir = ccc_archive::staging_profile_dir(&staging, &host, &profile);
            let st = ccc_archive::ingest_pulled(&conn, &dir, &host, &profile)?;
            println!(
                "pull 取り込み完了: host={host} profile={profile} \
                 files={} new_messages={} sessions={} memory_new_versions={}",
                st.files, st.new_messages, st.sessions, st.memory_new
            );
        }
        Cmd::Sync { .. } => {
            let root = claude_profiles_root()?;
            let (mut files, mut new, mut profiles) = (0usize, 0usize, 0usize);
            let (mut mem_scanned, mut mem_new) = (0usize, 0usize);
            if root.is_dir() {
                for entry in std::fs::read_dir(&root)?.flatten() {
                    let profile_dir = entry.path();
                    if !profile_dir.is_dir() {
                        continue;
                    }
                    profiles += 1;
                    let projects = profile_dir.join("projects");
                    if projects.is_dir() {
                        let s = ccc_archive::scan_projects(&conn, &projects)?;
                        files += s.files;
                        new += s.new_messages;
                    }
                    // メモリ資産も同時に保全（ローカル）。
                    let name = profile_dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("default");
                    let ms =
                        ccc_archive::snapshot_memory(&conn, &profile_dir, name, "local", None)?;
                    mem_scanned += ms.scanned;
                    mem_new += ms.new_versions;
                }
            }
            println!(
                "取り込み完了: profiles={profiles} files={files} new_messages={new} \
                 / memory: scanned={mem_scanned} new_versions={mem_new}"
            );
        }
        Cmd::Search {
            query,
            project,
            since,
            limit,
            json,
        } => {
            let hits =
                ccc_archive::search(&conn, &query, project.as_deref(), since_to_ms(since), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                eprintln!("ヒットなし: {query}");
            } else {
                for h in &hits {
                    let date = h.ts.map(fmt_ts).unwrap_or_else(|| "-".into());
                    let role = h.role.as_deref().unwrap_or("?");
                    let proj = h.project.as_deref().unwrap_or("-");
                    let snip = one_line(&h.snippet, 100);
                    let seq = h.seq.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
                    println!("{date}  {proj}  [{role}]  {snip}");
                    println!("        ↳ {}#{seq}", short(&h.session_id));
                }
            }
        }
        Cmd::List {
            kind,
            host,
            project,
            since,
            limit,
            json,
        } => {
            let filter = ccc_archive::ListFilter {
                kind,
                host,
                project,
                since_ms: since_to_ms(since),
                limit,
            };
            let sessions = ccc_archive::list_sessions(&conn, &filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                eprintln!("セッションなし（先に `ccc-sessions sync` を実行）");
            } else {
                for s in &sessions {
                    let date = s
                        .ended_at
                        .or(s.started_at)
                        .map(fmt_ts)
                        .unwrap_or_else(|| "-".into());
                    let k = kind_short(s.kind.as_deref());
                    let proj = truncate(s.project.as_deref().unwrap_or("-"), 18);
                    let title = truncate(s.summary.as_deref().unwrap_or(s.session_id.as_str()), 50);
                    println!(
                        "{date}  {k}  {proj:<18}  {title}  ({} msgs)  {}",
                        s.message_count,
                        short(&s.session_id)
                    );
                }
            }
        }
        Cmd::Show {
            session_id,
            full,
            raw,
            json,
        } => {
            // 表示は短縮 id（先頭8文字）なので、プレフィックス前方一致で解決。
            // 検索出力の `<sid>#<seq>` 形式もそのまま受け付ける（`#` 以降は無視）。
            let key = session_id.split('#').next().unwrap_or(&session_id);
            if key.is_empty() {
                // 空プレフィックスは LIKE '%' で全件一致してしまうため明示的に弾く。
                eprintln!("session-id（プレフィックス可）を指定してください");
                return Ok(());
            }
            let ids = ccc_archive::resolve_session_ids(&conn, key)?;
            let sid = match ids.as_slice() {
                [] => {
                    eprintln!("該当セッションなし: {key}");
                    return Ok(());
                }
                [one] => one.clone(),
                many => {
                    eprintln!(
                        "プレフィックス '{key}' が複数一致。完全な session-id を指定してください:"
                    );
                    for id in many {
                        eprintln!("  {id}");
                    }
                    return Ok(());
                }
            };
            let msgs = ccc_archive::show_session(&conn, &sid)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msgs)?);
                return Ok(());
            }
            if raw {
                for m in &msgs {
                    if let Some(r) = &m.raw {
                        println!("{r}");
                    }
                }
                return Ok(());
            }
            // 既定は読みやすい要約: 本文を短縮、thinking/空は省略、ツールは1行。
            let cap = if full { usize::MAX } else { 600 };
            let tr_cap = if full { usize::MAX } else { 160 };
            println!("# {sid}  ({} msgs)", msgs.len());
            for m in &msgs {
                let role = m.role.as_deref().unwrap_or("?");
                let side = if m.is_sidechain { "↪" } else { "" };
                let mt = m.msg_type.as_deref().unwrap_or("");
                let tool = m.tool_name.as_deref().unwrap_or("");
                match mt {
                    "thinking" => {}
                    "tool_use" => println!("  · {side}tool_use {tool}"),
                    "tool_result" => {
                        if let Some(t) = body_text(&m.text) {
                            println!("  · {side}tool_result: {}", one_line(&t, tr_cap));
                        }
                    }
                    _ => {
                        if let Some(t) = body_text(&m.text) {
                            println!("\n▸ {side}{role}:");
                            println!("{}", truncate_body(&t, cap));
                        }
                    }
                }
            }
        }
        Cmd::Recent {
            days,
            project,
            json,
        } => {
            let filter = ccc_archive::ListFilter {
                kind: None,
                host: None,
                project,
                since_ms: since_to_ms(Some(days)),
                limit: 1000,
            };
            let sessions = ccc_archive::list_sessions(&conn, &filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                eprintln!("直近 {days} 日の活動なし");
            } else {
                println!("直近 {days} 日の活動: {} セッション", sessions.len());
                let mut cur_date = String::new();
                for s in &sessions {
                    let stamp = s.ended_at.or(s.started_at).map(fmt_ts);
                    let (date, time) = match &stamp {
                        Some(f) => f.split_once(' ').unwrap_or((f.as_str(), "")),
                        None => ("-", "-"),
                    };
                    if date != cur_date {
                        println!("\n📅 {date}");
                        cur_date = date.to_string();
                    }
                    let k = kind_short(s.kind.as_deref());
                    let proj = truncate(s.project.as_deref().unwrap_or("-"), 16);
                    let title = truncate(s.summary.as_deref().unwrap_or(s.session_id.as_str()), 50);
                    println!(
                        "  {time}  {k}  {proj:<16}  {title}  ({} msgs)  {}",
                        s.message_count,
                        short(&s.session_id)
                    );
                }
            }
        }
        Cmd::Stats { json } => {
            let st = ccc_archive::stats(&conn)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "セッション: {}  (local {} / remote {})",
                    st.sessions, st.local_sessions, st.remote_sessions
                );
                println!("メッセージ: {}", st.messages);
                println!(
                    "メモリ: {} ファイル / {} 版",
                    st.memory_files, st.memory_versions
                );
                let range = match (st.first_activity, st.last_activity) {
                    (Some(a), Some(b)) => format!("{} 〜 {}", fmt_ts(a), fmt_ts(b)),
                    _ => "-".into(),
                };
                println!("期間: {range}");
                if !st.by_attribution.is_empty() {
                    let parts: Vec<String> = st
                        .by_attribution
                        .iter()
                        .map(|c| format!("{}={}", c.label.as_deref().unwrap_or("?"), c.count))
                        .collect();
                    println!("帰属: {}", parts.join("  "));
                }
                if !st.top_projects.is_empty() {
                    println!("プロジェクト上位:");
                    for p in &st.top_projects {
                        println!("  {:<20} {}", p.label.as_deref().unwrap_or("-"), p.count);
                    }
                }
            }
        }
        Cmd::Memory { cmd } => run_memory(&conn, cmd)?,
        Cmd::TrainDict {
            kind,
            capacity,
            samples,
            no_vacuum,
            batch,
        } => run_train_dict(&conn, &db, kind, capacity, samples, no_vacuum, batch)?,
    }
    Ok(())
}

// ── train-dict サブコマンド（v0.6 §9 / schema v4） ──

fn run_train_dict(
    conn: &ccc_archive::Connection,
    db_path: &Path,
    kind: String,
    capacity_kb: usize,
    samples: usize,
    no_vacuum: bool,
    batch: usize,
) -> anyhow::Result<()> {
    let targets: Vec<&'static str> = match kind.as_str() {
        "raw" => vec![ccc_archive::KIND_RAW],
        "payload" => vec![ccc_archive::KIND_PAYLOAD],
        "both" => vec![ccc_archive::KIND_RAW, ccc_archive::KIND_PAYLOAD],
        other => anyhow::bail!("kind は 'raw' | 'payload' | 'both' を指定: '{other}'"),
    };
    let capacity = capacity_kb * 1024;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    for k in &targets {
        eprintln!("\n[train-dict {k}] サンプル抽出（最大 {samples} 件）…");
        let sample_blobs = ccc_archive::collect_training_samples(conn, k, samples)?;
        let total_sample_bytes: usize = sample_blobs.iter().map(|b| b.len()).sum();
        eprintln!(
            "[train-dict {k}] サンプル={} 件 / 合計 {} を {} 上限で学習 + 全行 atomic 再圧縮…",
            sample_blobs.len(),
            human_bytes(total_sample_bytes as u64),
            human_bytes(capacity as u64),
        );
        let sample_refs: Vec<&[u8]> = sample_blobs.iter().map(|v| v.as_slice()).collect();
        let stats =
            ccc_archive::retrain_kind(conn, k, &sample_refs, capacity, batch, now_ms, |p| {
                eprintln!(
                    "  {}/{} ({:.0}%)  {} → {}",
                    p.processed,
                    p.total,
                    if p.total > 0 {
                        100.0 * p.processed as f64 / p.total as f64
                    } else {
                        100.0
                    },
                    human_bytes(p.bytes_before),
                    human_bytes(p.bytes_after),
                );
            })?;
        eprintln!(
            "[train-dict {k}] 完了: 辞書 {} / 再圧縮 {} 行  {} → {}（比 {:.1}%）",
            human_bytes(stats.trained.blob_len as u64),
            stats.recompress.processed,
            human_bytes(stats.recompress.bytes_before),
            human_bytes(stats.recompress.bytes_after),
            if stats.recompress.bytes_before > 0 {
                100.0 * stats.recompress.bytes_after as f64 / stats.recompress.bytes_before as f64
            } else {
                0.0
            },
        );
    }

    if !no_vacuum {
        let before = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "\n[train-dict] VACUUM 実行中 (前: {})…",
            human_bytes(before)
        );
        conn.execute_batch("VACUUM")?;
        let after = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "[train-dict] VACUUM 完了: {} → {}（{:.1}% 縮約）",
            human_bytes(before),
            human_bytes(after),
            if before > 0 {
                100.0 * (before.saturating_sub(after)) as f64 / before as f64
            } else {
                0.0
            },
        );
    }
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

// ── memory サブコマンド ──

fn run_memory(conn: &ccc_archive::Connection, cmd: MemoryCmd) -> anyhow::Result<()> {
    match cmd {
        MemoryCmd::List {
            profile,
            scope,
            project,
            json,
        } => {
            let filter = ccc_archive::MemoryFilter {
                profile,
                scope,
                project,
            };
            let entries = ccc_archive::list_memory(conn, &filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                eprintln!("メモリ記録なし（先に `ccc-sessions sync` を実行）");
            } else {
                for e in &entries {
                    let date = e.captured_at.map(fmt_ts).unwrap_or_else(|| "-".into());
                    let sc = scope_short(e.scope.as_deref());
                    let prof = e.agent_profile.as_deref().unwrap_or("-");
                    let vers = if e.versions > 1 {
                        format!("v{}", e.versions)
                    } else {
                        "  ".into()
                    };
                    println!("{date}  {sc}  {vers:>3}  {prof}/{}", e.rel_path);
                }
            }
        }
        MemoryCmd::Show {
            rel_path,
            profile,
            version,
            json,
        } => {
            let versions = resolve_versions(conn, profile.as_deref(), &rel_path)?;
            let v = pick_version(&versions, version.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(v)?);
            } else {
                print!("{}", v.content.as_deref().unwrap_or(""));
            }
        }
        MemoryCmd::Diff { rel_path, profile } => {
            let versions = resolve_versions(conn, profile.as_deref(), &rel_path)?;
            if versions.len() < 2 {
                eprintln!("差分なし（{} 版のみ）", versions.len());
                return Ok(());
            }
            let new = &versions[0];
            let old = &versions[1];
            let nh = short_hash(new.content_hash.as_deref());
            let oh = short_hash(old.content_hash.as_deref());
            let nd = new.captured_at.map(fmt_ts).unwrap_or_else(|| "-".into());
            let od = old.captured_at.map(fmt_ts).unwrap_or_else(|| "-".into());
            println!("--- {oh}  {od}");
            println!("+++ {nh}  {nd}");
            print!(
                "{}",
                ccc_archive::line_diff(
                    old.content.as_deref().unwrap_or(""),
                    new.content.as_deref().unwrap_or("")
                )
            );
        }
        MemoryCmd::Restore {
            rel_path,
            profile,
            version,
            yes,
        } => {
            let versions = resolve_versions(conn, profile.as_deref(), &rel_path)?;
            let target = pick_version(&versions, version.as_deref())?;
            let prof = target.agent_profile.clone().ok_or_else(|| {
                anyhow::anyhow!("この版に agent_profile がありません（復元先を特定できません）")
            })?;
            // prof / rel_path は DB（リモート pull 由来を含む半信頼入力）。`..` や絶対パスに
            // よる CLAUDE_CONFIG_DIR 外への書き込みを防ぐため、書き戻し先を検証する。
            let root = claude_profiles_root()?;
            let dest = validate_restore_dest(&root, &prof, &target.rel_path)?;
            let profile_dir = root.join(&prof); // prof は validate 済みで安全
            let hash = short_hash(target.content_hash.as_deref());
            let date = target.captured_at.map(fmt_ts).unwrap_or_else(|| "-".into());

            if !yes {
                eprintln!(
                    "復元先: {}\n  版: {hash}  ({date})\n  上書き前に現状を自動スナップショットします。",
                    dest.display()
                );
                eprintln!("実行するには --yes を付けてください。");
                return Ok(());
            }
            // 上書き前に現状を保全（archive に残るので .bak は作らない）。
            ccc_archive::snapshot_memory(conn, &profile_dir, &prof, "local", None)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, target.content.as_deref().unwrap_or(""))?;
            println!("復元しました: {} ← {hash} ({date})", dest.display());
        }
    }
    Ok(())
}

/// rel_path の版を解決する。profile 未指定で複数プロファイルに跨る場合は中断させる。
fn resolve_versions(
    conn: &ccc_archive::Connection,
    profile: Option<&str>,
    rel_path: &str,
) -> anyhow::Result<Vec<ccc_archive::MemoryVersion>> {
    let versions = ccc_archive::memory_versions(conn, profile, rel_path)?;
    if versions.is_empty() {
        anyhow::bail!("該当メモリなし: {rel_path}");
    }
    let mut profiles: Vec<&str> = versions
        .iter()
        .filter_map(|v| v.agent_profile.as_deref())
        .collect();
    profiles.sort();
    profiles.dedup();
    if profiles.len() > 1 {
        anyhow::bail!(
            "'{rel_path}' は複数プロファイルに存在します。--profile で指定してください: {}",
            profiles.join(", ")
        );
    }
    Ok(versions)
}

/// content-hash 前方一致で版を選ぶ。None なら最新（先頭）。
fn pick_version<'a>(
    versions: &'a [ccc_archive::MemoryVersion],
    hash_prefix: Option<&str>,
) -> anyhow::Result<&'a ccc_archive::MemoryVersion> {
    match hash_prefix {
        None => Ok(&versions[0]),
        Some(p) => versions
            .iter()
            .find(|v| v.content_hash.as_deref().is_some_and(|h| h.starts_with(p)))
            .ok_or_else(|| anyhow::anyhow!("version '{p}' に一致する版がありません")),
    }
}

// ── パス解決（ccc の paths.rs 規約に準拠。archive は dev 別、agent_settings は共有）──

fn is_dev() -> bool {
    matches!(std::env::var("CCC_DEV").as_deref(), Ok(v) if !v.is_empty() && v != "0")
}

fn home() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME 未設定")?))
}

fn db_path() -> anyhow::Result<PathBuf> {
    let mut root = home()?.join(".ccc");
    if is_dev() {
        root = root.join("dev");
    }
    Ok(root.join("archive").join("sessions.db"))
}

fn claude_profiles_root() -> anyhow::Result<PathBuf> {
    Ok(home()?.join(".ccc").join("agent_settings").join("claude"))
}

/// メモリ復元の書き戻し先 `<root>/<profile>/<rel_path>` を組み立てつつ検証する。
/// `profile`・`rel_path` は DB 由来（リモート pull 由来を含む半信頼入力）のため、
/// 通常成分以外（`..`・絶対パス・ルート・ドライブ接頭辞）を拒否して `root` 配下に
/// 収まることを保証する。認証情報ファイルも対象外とする。
fn validate_restore_dest(root: &Path, profile: &str, rel_path: &str) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let mut dest = root.to_path_buf();
    for seg in [profile, rel_path] {
        for comp in Path::new(seg).components() {
            match comp {
                Component::Normal(p) => dest.push(p),
                _ => anyhow::bail!(
                    "不正なパス成分（復元先がプロファイル外へ脱出します）: profile={profile} rel_path={rel_path}"
                ),
            }
        }
    }
    if dest.file_name().and_then(|s| s.to_str()) == Some(".credentials.json") {
        anyhow::bail!("認証情報ファイルは復元対象外です");
    }
    Ok(dest)
}

/// リモート pull のステージング先ルート `~/.ccc[/dev]/archive/pulled/`。
fn staging_root() -> anyhow::Result<PathBuf> {
    let mut root = home()?.join(".ccc");
    if is_dev() {
        root = root.join("dev");
    }
    Ok(root.join("archive").join("pulled"))
}

// ── 表示ヘルパ ──

fn since_to_ms(days: Option<i64>) -> Option<i64> {
    days.map(|d| now_ms() - d * 86_400_000)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn kind_short(kind: Option<&str>) -> &'static str {
    match kind {
        Some("local") => "L",
        Some("remote") => "R",
        _ => "·",
    }
}

fn scope_short(scope: Option<&str>) -> &'static str {
    match scope {
        Some("user") => "U",
        Some("project") => "P",
        _ => "·",
    }
}

fn short_hash(hash: Option<&str>) -> String {
    hash.map(|h| h.chars().take(8).collect())
        .unwrap_or_else(|| "-".into())
}

fn short(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head: String = chars.into_iter().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max)
}

/// 空白のみ/None を除いた本文。
fn body_text(t: &Option<String>) -> Option<String> {
    t.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 改行を保ちつつ文字数で切り詰め、超過分を注記する。
fn truncate_body(s: &str, cap: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        return s.to_string();
    }
    let head: String = chars.iter().take(cap).collect();
    format!("{head}…（+{}字）", chars.len() - cap)
}

/// unix ms → "YYYY-MM-DD HH:MM"（UTC）。
fn fmt_ts(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Howard Hinnant の civil_from_days（1970-01-01 起点の日数 → 年月日）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_restore_dest_allows_normal_path() {
        let root = Path::new("/tmp/claude");
        let dest = validate_restore_dest(root, "default", "projects/enc/memory/MEMORY.md").unwrap();
        assert_eq!(
            dest,
            Path::new("/tmp/claude/default/projects/enc/memory/MEMORY.md")
        );
    }

    #[test]
    fn validate_restore_dest_rejects_parent_escape() {
        let root = Path::new("/tmp/claude");
        assert!(validate_restore_dest(root, "default", "../../etc/passwd").is_err());
        assert!(validate_restore_dest(root, "default", "a/../../b.md").is_err());
        assert!(validate_restore_dest(root, "..", "CLAUDE.md").is_err());
    }

    #[test]
    fn validate_restore_dest_rejects_absolute() {
        let root = Path::new("/tmp/claude");
        assert!(validate_restore_dest(root, "default", "/etc/passwd").is_err());
    }

    #[test]
    fn validate_restore_dest_rejects_credentials() {
        let root = Path::new("/tmp/claude");
        assert!(validate_restore_dest(root, "default", ".credentials.json").is_err());
        assert!(validate_restore_dest(root, "default", "sub/.credentials.json").is_err());
    }
}
