//! セッション/メモリを SQLite に集約する書き込みサービス。
//!
//! `ccc-archive` の DB 接続を**専用スレッドが 1 本だけ所有**し（単一 writer）、
//! `std::sync::mpsc` で非ブロッキングに受けたジョブを順に処理する。hook 受信ループ
//! （`lib.rs`）から `record_hook` を呼ぶだけで、作業中にライブ取り込みされる。
//!
//! - 全 hook → `events` 追記＋インスタンス活動窓更新
//! - `SessionStart` → `instances`/`sessions` メタ確定（`attribution='hook'`）
//! - ローカルインスタンスで `transcript_path` が読めれば増分取り込み
//! - リモートインスタンスは rsync pull（境界＝Stop/SessionEnd で狙い撃ち、定期＝活動ゲート、
//!   切断/終了で全面 sweep）。**rsync 自体は別スレッド**で実行し、取り込み（DB 書き込み）
//!   だけを単一 writer に積むことで、ネットワーク待ちが他の集約を止めないようにする。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ccc_archive::{Connection, InstanceMeta};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::instance::notify::emit_status_changed;
use crate::instance::storage;
use crate::instance::types::{InstanceId, InstanceInfo};

/// hook 1 件分の取り込みジョブ。
pub struct HookJob {
    pub hook_event: String,
    pub payload: Value,
    pub meta: InstanceMeta,
    /// ローカルインスタンスか（true のときだけ transcript_path をローカル読みする）
    pub is_local: bool,
}

/// リモート pull の依頼。
pub struct PullRequest {
    pub host_alias: String,
    pub profile: String,
    /// `Some(remote_transcript_path)` = 狙い撃ち、`None` = 全面 sweep。
    pub transcript_path: Option<String>,
}

enum Job {
    Hook(Box<HookJob>),
    /// `~/.ccc/agent_settings/claude/` 配下の全プロファイルのメモリを一括スナップショット。
    ScanMemory {
        claude_root: PathBuf,
    },
    /// 全プロファイルのローカル transcript をフルスキャンして取り込む（起動時バックフィル）。
    /// hook 喪失期間（ccc 未起動中・hook endpoint が別プロセスを向いていた期間）の
    /// セッションを回収し、起点 cwd のズレも修復する。増分カーソルにより 2 回目以降は安価。
    ScanTranscripts {
        claude_root: PathBuf,
    },
    /// rsync pull 済みステージングを取り込む（DB 書き込み。rsync 完了後に積まれる）。
    IngestPulled {
        staging_profile_dir: PathBuf,
        host_alias: String,
        profile: String,
    },
}

/// 定期同期サイクルの間隔（秒）。UI のステータスライン表示にも使う。
pub const SYNC_INTERVAL_SECS: u64 = 60;
/// 定期 pull の間隔（活動があったホストのみ。アイドルはスキップ）。
const PERIODIC_INTERVAL: Duration = Duration::from_secs(SYNC_INTERVAL_SECS);

/// 最終同期サイクル時刻（unix ms。0 = 未実施）を UI と共有するためのハンドル。
pub type LastSyncAt = Arc<AtomicI64>;

/// `archive-sync` イベントのペイロード（同期サイクルのハートビート）。
#[derive(Clone, Serialize)]
struct SyncTick {
    /// このサイクルの時刻（unix ms）。
    at: i64,
    /// 次サイクルまでの想定間隔（秒）。
    interval_secs: u64,
    /// このサイクルで pull を起動したホスト数。
    pulled_hosts: usize,
}

/// 書き込みサービスのハンドル（送信側）。clone 可。
#[derive(Clone)]
pub struct ArchiveService {
    tx: Sender<Job>,
    /// pull のステージング先ルート `~/.ccc[/dev]/archive/pulled/`。
    staging_root: PathBuf,
    /// (host, profile) 単位の pull 単一フライト制御（並行 pull を 1 本化）。key は dirty と同形式。
    pulling: Arc<DashMap<String, ()>>,
    /// 前回 pull 以降にリモート活動があった (host, profile)。定期 pull の活動ゲート。
    /// key = "<host>\0<profile>"。
    dirty: Arc<DashMap<String, (String, String)>>,
    /// フロントへ同期ハートビートを emit するためのハンドル（無ければ emit しない）。
    app_handle: Option<AppHandle>,
    /// 最終同期サイクル時刻（UI のステータスライン用）。
    last_sync: LastSyncAt,
}

impl ArchiveService {
    /// DB を開き、書き込み専用スレッドと定期 pull スレッドを起動する。
    /// `app_handle` があれば各同期サイクルで `archive-sync` を emit し、`last_sync` を更新する。
    /// `infos` は ingest 完了後にサイドバー「セッションタイトル」を反映するための
    /// `InstanceInfo` ストア共有ハンドル。
    pub fn start(
        db_path: PathBuf,
        app_handle: Option<AppHandle>,
        last_sync: LastSyncAt,
        infos: Arc<DashMap<InstanceId, InstanceInfo>>,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 起動時刻で同期クロックを初期化（UI が初回マウント時に「同期 N 秒前」を出せる）。
        last_sync.store(now_ms(), Ordering::Relaxed);
        let staging_root = db_path
            .parent()
            .map(|p| p.join("pulled"))
            .unwrap_or_else(|| PathBuf::from("pulled"));

        let conn = ccc_archive::open(&db_path)?;
        let (tx, rx): (Sender<Job>, Receiver<Job>) = std::sync::mpsc::channel();
        let dirty: Arc<DashMap<String, (String, String)>> = Arc::new(DashMap::new());

        // 単一 writer スレッド: 接続を所有し、ジョブを直列に処理する。
        let writer_dirty = dirty.clone();
        let writer_infos = infos; // writer が唯一の所有者として動く（呼出側は保持しない）
        let writer_app_handle = app_handle.clone();
        std::thread::Builder::new()
            .name("ccc-archive-writer".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let j = match job {
                        Job::Hook(j) => j,
                        Job::ScanMemory { claude_root } => {
                            scan_all_profiles_memory(&conn, &claude_root);
                            continue;
                        }
                        Job::ScanTranscripts { claude_root } => {
                            scan_all_profiles_transcripts(&conn, &claude_root);
                            continue;
                        }
                        Job::IngestPulled {
                            staging_profile_dir,
                            host_alias,
                            profile,
                        } => {
                            match ccc_archive::ingest_pulled(
                                &conn,
                                &staging_profile_dir,
                                &host_alias,
                                &profile,
                            ) {
                                Ok(s) => eprintln!(
                                    "[ccc-archive] pull 取込: host={host_alias} profile={profile} \
                                     files={} msgs={} sessions={} memory={}",
                                    s.files, s.new_messages, s.sessions, s.memory_new
                                ),
                                Err(e) => {
                                    eprintln!("[ccc-archive] pull 取込失敗 ({host_alias}): {e}")
                                }
                            }
                            // pull 取込完了後、該当ホストのリモートインスタンスについて
                            // 現在ライブの session_id の summary を引き直してサイドバー反映。
                            refresh_remote_session_titles(
                                &conn,
                                &writer_infos,
                                &writer_app_handle,
                                &host_alias,
                            );
                            continue;
                        }
                    };
                    let ts = now_ms();
                    let session_id = j.payload.get("session_id").and_then(Value::as_str);

                    if let Err(e) = ccc_archive::record_event(
                        &conn,
                        &j.meta.instance_id,
                        session_id,
                        &j.hook_event,
                        ts,
                        &j.payload,
                    ) {
                        eprintln!("[ccc-archive] record_event 失敗: {e}");
                    }

                    if j.hook_event == "SessionStart" {
                        if let Some(sid) = session_id {
                            let source = j.payload.get("source").and_then(Value::as_str);
                            if let Err(e) =
                                ccc_archive::record_session_start(&conn, &j.meta, sid, source, ts)
                            {
                                eprintln!("[ccc-archive] record_session_start 失敗: {e}");
                            }
                        }
                    }

                    if j.is_local {
                        // ローカル: transcript をその場で増分取り込み。
                        if let Some(tp) = j.payload.get("transcript_path").and_then(Value::as_str) {
                            let path = Path::new(tp);
                            if path.exists() {
                                if let Err(e) = ccc_archive::ingest_transcript(&conn, path) {
                                    eprintln!("[ccc-archive] ingest 失敗 ({tp}): {e}");
                                }
                            }
                        }
                        // ingest 完了直後、現在の session_id について summary を引き直し、
                        // 該当インスタンスのサイドバー表示を更新する（Local/Remote 共通経路）。
                        if let Some(sid) = session_id {
                            refresh_session_title(&conn, &writer_infos, &writer_app_handle, sid);
                        }
                        // SessionEnd でそのプロファイルのメモリを保全（揮発前のスナップショット）。
                        if j.hook_event == "SessionEnd" {
                            if let Some(profile) = j.meta.agent_profile.as_deref() {
                                snapshot_profile_memory(&conn, profile);
                            }
                        }
                    } else if let (Some(host), Some(profile)) = (
                        j.meta.host_alias.as_deref(),
                        j.meta.agent_profile.as_deref(),
                    ) {
                        // リモート: 活動ゲート用に dirty マーク（定期 pull が拾う）。
                        writer_dirty.insert(
                            fly_key(host, profile),
                            (host.to_string(), profile.to_string()),
                        );
                    }
                }
            })?;

        let svc = Self {
            tx,
            staging_root,
            pulling: Arc::new(DashMap::new()),
            dirty,
            app_handle,
            last_sync,
        };

        // 定期 pull スレッド: 活動があった (host, profile) だけを sweep pull する。
        svc.spawn_periodic();
        Ok(svc)
    }

    /// hook を取り込みキューに積む（非ブロッキング。スレッド停止時は黙って捨てる）。
    pub fn record_hook(&self, job: HookJob) {
        let _ = self.tx.send(Job::Hook(Box::new(job)));
    }

    /// 全ローカルプロファイルのメモリスナップショットをキューに積む（起動時に 1 回）。
    pub fn scan_memory(&self, claude_root: PathBuf) {
        let _ = self.tx.send(Job::ScanMemory { claude_root });
    }

    /// 全ローカルプロファイルの transcript フルスキャンをキューに積む（起動時に 1 回）。
    pub fn scan_transcripts(&self, claude_root: PathBuf) {
        let _ = self.tx.send(Job::ScanTranscripts { claude_root });
    }

    /// リモート pull を依頼する（rsync は別スレッド、取り込みは writer）。
    /// 同一 (host, profile) の pull が走行中なら何もしない（単一フライト）。
    pub fn record_pull(&self, req: PullRequest) {
        spawn_pull(
            self.tx.clone(),
            self.pulling.clone(),
            self.staging_root.clone(),
            req,
        );
    }

    /// 定期 pull スレッドを起動する。各サイクルで同期ハートビートを emit する。
    fn spawn_periodic(&self) {
        let tx = self.tx.clone();
        let pulling = self.pulling.clone();
        let staging = self.staging_root.clone();
        let dirty = self.dirty.clone();
        let app_handle = self.app_handle.clone();
        let last_sync = self.last_sync.clone();
        std::thread::Builder::new()
            .name("ccc-archive-pull-tick".into())
            .spawn(move || loop {
                std::thread::sleep(PERIODIC_INTERVAL);
                // 活動ゲート: dirty が空ならスキップ。あった分だけ sweep。
                let due: Vec<(String, String)> = dirty.iter().map(|e| e.value().clone()).collect();
                let mut pulled_hosts = 0usize;
                for (host, profile) in due {
                    let started = spawn_pull(
                        tx.clone(),
                        pulling.clone(),
                        staging.clone(),
                        PullRequest {
                            host_alias: host.clone(),
                            profile: profile.clone(),
                            transcript_path: None,
                        },
                    );
                    // 起動できた分だけ dirty を消す。走行中/起動失敗なら残し次サイクルで再試行。
                    if started {
                        dirty.remove(&fly_key(&host, &profile));
                        pulled_hosts += 1;
                    }
                }
                // 同期サイクルのハートビート（UI のステータスライン用）。
                let now = now_ms();
                last_sync.store(now, Ordering::Relaxed);
                if let Some(h) = &app_handle {
                    let _ = h.emit(
                        "archive-sync",
                        SyncTick {
                            at: now,
                            interval_secs: SYNC_INTERVAL_SECS,
                            pulled_hosts,
                        },
                    );
                }
            })
            .ok();
    }
}

/// rsync pull を別スレッドで実行し、成功したら取り込みジョブを writer に積む。
/// `(host, profile)` 単位の単一フライト。新規に起動できたら `true`、既に走行中
/// またはスレッド起動失敗なら `false` を返す（呼び出し側が dirty を残して再試行できる）。
fn spawn_pull(
    tx: Sender<Job>,
    pulling: Arc<DashMap<String, ()>>,
    staging_root: PathBuf,
    req: PullRequest,
) -> bool {
    // 同一 (host, profile) を pull 中なら重複起動しない。host 単位だと同一ホストの
    // 別プロファイルの pull が取りこぼされるため、dirty と同じ粒度で制御する。
    let key = fly_key(&req.host_alias, &req.profile);
    if pulling.insert(key.clone(), ()).is_some() {
        return false;
    }
    let host_for_log = req.host_alias.clone(); // req は closure に move されるためログ用に控える
    let spawned = {
        let pulling = pulling.clone();
        let key = key.clone();
        std::thread::Builder::new()
            .name("ccc-archive-pull".into())
            .spawn(move || {
                let res = match &req.transcript_path {
                    Some(tp) => {
                        ccc_archive::pull_session(&req.host_alias, &req.profile, tp, &staging_root)
                    }
                    None => ccc_archive::pull_profile(&req.host_alias, &req.profile, &staging_root),
                };
                match res {
                    Ok(()) => {
                        let dir = ccc_archive::staging_profile_dir(
                            &staging_root,
                            &req.host_alias,
                            &req.profile,
                        );
                        let _ = tx.send(Job::IngestPulled {
                            staging_profile_dir: dir,
                            host_alias: req.host_alias.clone(),
                            profile: req.profile.clone(),
                        });
                    }
                    Err(e) => eprintln!("[ccc-archive] pull 失敗 ({}): {e}", req.host_alias),
                }
                pulling.remove(&key);
            })
    };
    if spawned.is_err() {
        // スレッドを起動できなかった。フライトキーを残すと以降その (host, profile) が
        // 永久に pull 不能になるため除去する。
        pulling.remove(&key);
        eprintln!("[ccc-archive] pull スレッド起動失敗 ({host_for_log})");
        return false;
    }
    true
}

/// pull 単一フライト・dirty で使う `(host, profile)` キー（NUL 区切り）。
fn fly_key(host: &str, profile: &str) -> String {
    format!("{host}\u{0}{profile}")
}

/// `claude_root`（`agent_settings/claude/`）配下の各プロファイルのメモリを保全する。
fn scan_all_profiles_memory(conn: &ccc_archive::Connection, claude_root: &Path) {
    let entries = match std::fs::read_dir(claude_root) {
        Ok(e) => e,
        Err(_) => return, // まだプロファイルが無いだけ。無害。
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Some(profile) = dir.file_name().and_then(|s| s.to_str()) {
            if let Err(e) = ccc_archive::snapshot_memory(conn, &dir, profile, "local", None) {
                eprintln!("[ccc-archive] memory スキャン失敗 ({profile}): {e}");
            }
        }
    }
}

/// `claude_root`（`agent_settings/claude/`）配下の各プロファイルのローカル transcript を
/// フルスキャンして取り込む。hook が届かなかった期間のセッションのバックフィル用。
fn scan_all_profiles_transcripts(conn: &ccc_archive::Connection, claude_root: &Path) {
    let entries = match std::fs::read_dir(claude_root) {
        Ok(e) => e,
        Err(_) => return, // まだプロファイルが無いだけ。無害。
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(profile) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        match ccc_archive::scan_projects(conn, &dir.join("projects")) {
            Ok(s) if s.new_messages > 0 => eprintln!(
                "[ccc-archive] 起動時スキャン: profile={profile} files={} msgs={}",
                s.files, s.new_messages
            ),
            Ok(_) => {}
            Err(e) => eprintln!("[ccc-archive] 起動時スキャン失敗 ({profile}): {e}"),
        }
    }
}

/// 単一プロファイル（`agent_settings/claude/<profile>/`）のメモリを保全する。
fn snapshot_profile_memory(conn: &ccc_archive::Connection, profile: &str) {
    let dir = match crate::paths::claude_agent_settings_dir(profile) {
        Ok(d) => d,
        Err(_) => return,
    };
    if let Err(e) = ccc_archive::snapshot_memory(conn, &dir, profile, "local", None) {
        eprintln!("[ccc-archive] memory スナップショット失敗 ({profile}): {e}");
    }
}

/// `session_id` の最新 summary（aiTitle 優先・なければ最初のユーザー入力）を archive DB から
/// 引き直し、`current_session_id == session_id` の `InstanceInfo` に書き戻して
/// `instance-status-changed` を emit する。Local/Remote 共通の経路。
///
/// Sessions タブの `summary` と同じソースを使うので、サイドバーとの文字列一致が
/// コードレベルで保証される。summary がまだ NULL（ingest 完了前 or aiTitle/first_user
/// どちらも未抽出）の場合は何もしない（既存値を保持）。
fn refresh_session_title(
    conn: &Connection,
    infos: &DashMap<InstanceId, InstanceInfo>,
    app_handle: &Option<AppHandle>,
    session_id: &str,
) {
    let summary = match ccc_archive::session_summary(conn, session_id) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            eprintln!("[ccc-archive] session_summary 取得失敗 ({session_id}): {e}");
            return;
        }
    };
    for mut entry in infos.iter_mut() {
        if entry.current_session_id.as_deref() != Some(session_id) {
            continue;
        }
        if entry.session_title.as_deref() == Some(summary.as_str()) {
            continue; // 変化なし。emit を抑制してフロントの無駄な更新を避ける。
        }
        entry.session_title = Some(summary.clone());
        let _ = storage::save_connection(&entry);
        emit_status_changed(app_handle, &entry);
    }
}

/// `host_alias` 配下のリモートインスタンス全てについて、現在の `current_session_id` で
/// summary を引き直す（rsync pull → ingest 完了後に呼ぶ）。
fn refresh_remote_session_titles(
    conn: &Connection,
    infos: &DashMap<InstanceId, InstanceInfo>,
    app_handle: &Option<AppHandle>,
    host_alias: &str,
) {
    let targets: Vec<String> = infos
        .iter()
        .filter(|e| e.host_alias.as_deref() == Some(host_alias))
        .filter_map(|e| e.current_session_id.clone())
        .collect();
    for sid in targets {
        refresh_session_title(conn, infos, app_handle, &sid);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
