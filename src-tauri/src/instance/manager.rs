use dashmap::DashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tauri::ipc::Channel;
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

use super::agent_config;
use super::consts::*;
use super::debug_log;
use super::hook_dispatch;
use super::pty_instance::{
    capture_tmux_scrollback, local_tmux_session_exists, HookEnv, PtyInstance, ScrollbackTarget,
};
use super::relay::{self, RelayContext};
use super::screen_monitor::{self, ScreenMonitor};
use super::storage;
use super::types::{InstanceId, InstanceInfo, InstanceKind, InstanceStatus, OutputPayload};
use super::watchdog;
use crate::hook_receiver::{HookEventKind, ReceivedHook};
use crate::ssh_config::{self, SshHost};

type OutputBroadcast = broadcast::Sender<Vec<u8>>;

type CancelTx = oneshot::Sender<()>;

/// 任意の id のインスタンスの status を更新し、永続化と
/// `instance-status-changed` event 発火を行う。
/// バックグラウンドタスクから restore 完了時に呼ぶ用途。
fn update_status_and_notify(
    infos: &DashMap<InstanceId, InstanceInfo>,
    app_handle: &Option<tauri::AppHandle>,
    id: &str,
    new_status: InstanceStatus,
) {
    let snapshot = if let Some(mut info_ref) = infos.get_mut(id) {
        info_ref.status = new_status;
        let _ = storage::save_connection(&info_ref);
        info_ref.clone()
    } else {
        return;
    };
    super::notify::emit_status_changed(app_handle, &snapshot);
}

/// SSH ready 信号 (Phase 1/Phase 2 完了通知) を待ち、結果に応じてログを記録した上で
/// 失敗時はエラーメッセージを返す。create_remote / reconnect / restore で共有。
async fn await_ssh_ready(
    ready_rx: oneshot::Receiver<Result<(), String>>,
    log_path: Option<&Path>,
    label: &str,
) -> Result<(), String> {
    match ready_rx.await {
        Ok(Ok(())) => {
            debug_log::append(log_path, &format!("[{label}] SSH接続確立 → Running"));
            Ok(())
        }
        Ok(Err(msg)) => {
            debug_log::append(log_path, &format!("[{label}] SSH接続失敗: {msg}"));
            Err(msg)
        }
        Err(_) => {
            debug_log::append(log_path, &format!("[{label}] PTYプロセス予期せず終了"));
            Err("PTY プロセスが予期せず終了しました".to_string())
        }
    }
}

/// 全インスタンスを一元管理する
pub struct InstanceManager {
    instances: Arc<DashMap<InstanceId, PtyInstance>>,
    infos: Arc<DashMap<InstanceId, InstanceInfo>>,
    broadcasts: Arc<DashMap<InstanceId, OutputBroadcast>>,
    cancel_txs: Arc<DashMap<InstanceId, CancelTx>>,
    app_handle: OnceLock<tauri::AppHandle>,
    /// 初回 subscribe 時に配信するスクロールバックデータ
    pending_scrollback: Arc<DashMap<InstanceId, Vec<u8>>>,
    /// Terminal タブ（shell）用 PTY。Agent と同じ tmux session の session-group メンバーに
    /// アタッチして shell window を表示する。Terminal タブを開いた時に lazy 起動する。
    shell_instances: Arc<DashMap<InstanceId, PtyInstance>>,
    shell_broadcasts: Arc<DashMap<InstanceId, OutputBroadcast>>,
    shell_cancel_txs: Arc<DashMap<InstanceId, CancelTx>>,
    shell_pending_scrollback: Arc<DashMap<InstanceId, Vec<u8>>>,
    /// HookReceiver の接続情報。`CCC_HOOK_ENDPOINT` / `CCC_SESSION_TOKEN` 自体は
    /// tmux 焼き込みではなく `~/.ccc/bin/ccc-hook.sh` ラッパー経由で注入するが、
    /// リモートに wrapper を scp で push する際にこの値を読み出す。
    hook_endpoint: OnceLock<String>,
    hook_token: OnceLock<String>,
    /// `ssh -R <port>:127.0.0.1:<port>` の port 番号（リモート逆方向転送用）
    hook_port: OnceLock<u16>,
    /// host_alias ごとの ControlMaster 確立シリアル化用ロック。
    /// `ensure_remote_master` の「存在確認 → 不健全なら立て直し」をホスト単位で
    /// アトミックにし、同一ホスト宛の並行 reattach のレースを排除する。
    ssh_master_locks: Arc<DashMap<String, Arc<AsyncMutex<()>>>>,
    /// ccc 自身が立てた ControlMaster の host_alias 集合。
    /// アプリ終了時にここに登録された alias へ `ssh -O exit` を発火して
    /// master プロセスを明示的に閉じる（`ControlPersist=30` 任せにしない）。
    ssh_masters_owned: Arc<DashMap<String, ()>>,
    /// セッション/メモリ集約の書き込みサービス（単一 writer）。
    /// `lib.rs` の setup で DB を開いて注入する。未設定なら archive は無効。
    archive: OnceLock<crate::archive_service::ArchiveService>,
    /// インスタンスごとの最終 hook 受信時刻（ウォッチドッグの無音判定用、非永続）。
    last_hook_at: Arc<DashMap<InstanceId, Instant>>,
    /// インスタンスごとの最終適用イベントの送信側時刻（順序逆転検出用、非永続）。
    last_hook_sent_at: Arc<DashMap<InstanceId, u64>>,
    /// ウォッチドッグ確認タスクの多重起動防止（非永続）。
    watchdog_suspects: Arc<DashMap<InstanceId, Instant>>,
    /// インスタンスごとのシャドウスクリーン（v0.12 画面状態検出、非永続）。
    /// relay 起動時に生成され、評価タスクが定期巡回する。
    screen_monitors: Arc<DashMap<InstanceId, Arc<Mutex<ScreenMonitor>>>>,
    /// 画面補正が設定した status の控え（hook 適用でクリア、非永続）。
    screen_set: Arc<DashMap<InstanceId, InstanceStatus>>,
    /// ホストごとの half-open/wedged 連続検知回数（v0.13、非永続）。
    /// スリープ復帰直後等の偽陽性で健全な master を畳まないよう、
    /// 連続 2 サイクル（約 2 分）で初めて復旧処理に入る。
    resilience_strikes: Arc<DashMap<String, u32>>,
    /// ホストごとの gpg agent forward 疎通状態（60 秒監視ループが更新、非永続）。
    /// UI がバッジ表示 / Tauri command `get_gpg_forward_status` で取得する。
    forward_status: Arc<DashMap<String, ccc_sshkit::agent_socket::ForwardHealth>>,
}

impl InstanceManager {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(DashMap::new()),
            infos: Arc::new(DashMap::new()),
            broadcasts: Arc::new(DashMap::new()),
            cancel_txs: Arc::new(DashMap::new()),
            app_handle: OnceLock::new(),
            pending_scrollback: Arc::new(DashMap::new()),
            shell_instances: Arc::new(DashMap::new()),
            shell_broadcasts: Arc::new(DashMap::new()),
            shell_cancel_txs: Arc::new(DashMap::new()),
            shell_pending_scrollback: Arc::new(DashMap::new()),
            hook_endpoint: OnceLock::new(),
            hook_token: OnceLock::new(),
            hook_port: OnceLock::new(),
            ssh_master_locks: Arc::new(DashMap::new()),
            ssh_masters_owned: Arc::new(DashMap::new()),
            archive: OnceLock::new(),
            last_hook_at: Arc::new(DashMap::new()),
            last_hook_sent_at: Arc::new(DashMap::new()),
            watchdog_suspects: Arc::new(DashMap::new()),
            screen_monitors: Arc::new(DashMap::new()),
            screen_set: Arc::new(DashMap::new()),
            resilience_strikes: Arc::new(DashMap::new()),
            forward_status: Arc::new(DashMap::new()),
        }
    }

    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        let _ = self.app_handle.set(handle);
    }

    /// 集約サービスを注入する（setup で 1 回）。
    pub fn set_archive(&self, archive: crate::archive_service::ArchiveService) {
        let _ = self.archive.set(archive);
    }

    /// archive_service が ingest 完了後に session_title を書き込めるように、
    /// `InstanceInfo` ストアへの共有ハンドルを返す（Arc は cheap clone）。
    pub fn infos_handle(&self) -> Arc<DashMap<InstanceId, InstanceInfo>> {
        Arc::clone(&self.infos)
    }

    /// HookReceiver の接続情報を保持する。
    /// インスタンス起動時に CCC_HOOK_ENDPOINT / CCC_SESSION_TOKEN として注入する。
    pub fn set_hook_endpoint(&self, endpoint: String, token: String, port: u16) {
        let _ = self.hook_endpoint.set(endpoint);
        let _ = self.hook_token.set(token);
        let _ = self.hook_port.set(port);
    }

    pub fn hook_port(&self) -> Option<u16> {
        self.hook_port.get().copied()
    }

    /// リモートへ wrapper script を push する際に必要な接続情報のスナップショット。
    /// `set_hook_endpoint` 前に呼ばれた場合は `None`。
    pub fn hook_credentials(&self) -> Option<(String, String)> {
        let endpoint = self.hook_endpoint.get()?.clone();
        let token = self.hook_token.get()?.clone();
        Some((endpoint, token))
    }

    /// 該当 host_alias 宛の ControlMaster + reverse forward が利用可能なことを保証する。
    ///
    /// 戻り値:
    /// - `Ok(true)`  = ccc 専用 master を利用可能（`-S <CCC_CONTROL_PATH>` で multiplex する）
    /// - `Ok(false)` = ユーザー側 ControlMaster を利用（ccc は何もしない）
    /// - `Err(_)`    = master 確立不可（呼び出し側で `Disconnected` にする想定）
    pub async fn ensure_remote_master(
        &self,
        host_alias: &str,
        log_path: Option<&Path>,
    ) -> Result<bool, String> {
        let port = self
            .hook_port()
            .ok_or_else(|| "hook_port 未確定（HookReceiver 起動前）".to_string())?;
        let result = ensure_remote_master_impl(
            host_alias,
            port,
            &self.ssh_master_locks,
            &self.ssh_masters_owned,
            log_path,
        )
        .await;

        // master が利用可能になったら port forward 台帳をリプレイする
        // （検知トリガー①。冪等なので fire-and-forget でよい）。
        // ユーザー CM 尊重（Ok(false)）でも master の世代交代はあり得るため両方で行う。
        if result.is_ok() {
            let alias = host_alias.to_string();
            tokio::task::spawn_blocking(move || crate::forwards::sync_ledger(&alias));
        }
        result
    }

    /// 特定ホストの gpg agent forward 疎通状態のスナップショット（UI 初期取得用）。
    /// 60 秒監視ループがまだ判定していないホストは `None`。
    pub fn forward_status_snapshot(
        &self,
        host_alias: &str,
    ) -> Option<ccc_sshkit::agent_socket::ForwardHealth> {
        self.forward_status.get(host_alias).map(|v| *v.value())
    }

    /// 60 秒監視ループの `Alive` 分岐で呼ぶ、gpg agent forward の
    /// 「軽量プローブ → broken なら無条件自動 heal → 状態遷移で emit」オーケストレーション。
    ///
    /// - probe は `ccc_sshkit::agent_socket::probe_agent_forward`（既存 `check` 相当、
    ///   世代ゲートなし）。コストは mux 経由 1 コマンド実行で ~100–200ms
    /// - broken 検知時: `ensure_agent_forward(force=true)` を呼び、内部の
    ///   `repair`（gpgconf --kill + rm -f + -O cancel/-O forward -R）に修復させる。
    ///   クールダウンは掛けない（ユーザー明示要望）。ストーム防止は後段の check → repair
    ///   で「healthy に戻ったら repair をスキップする」冪等性に依拠
    /// - 状態遷移（前回と異なる）で Tauri event `gpg-forward-status-changed` を emit
    async fn probe_and_maybe_heal(&self, host_alias: &str) {
        use ccc_sshkit::agent_socket::{self, ForwardHealth};

        let host = host_alias.to_string();
        let health = tokio::task::spawn_blocking(move || {
            agent_socket::probe_agent_forward(&host, &|msg| eprintln!("[ccc] {msg}"))
        })
        .await
        .unwrap_or(ForwardHealth::Unreachable);

        // 状態遷移を検出（初回は Some で必ず emit）
        let previous = self.forward_status.insert(host_alias.to_string(), health);
        let transitioned = previous != Some(health);

        // broken なら無条件自動 heal（クールダウンなし）
        let auto_heal_triggered = matches!(health, ForwardHealth::Broken);
        if auto_heal_triggered {
            eprintln!(
                "[ccc] [monitor] {host_alias}: gpg forward broken 検知 → 自動 heal 発火"
            );
            let host = host_alias.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                crate::agent_socket::ensure_agent_forward(&host, None);
            })
            .await;
            // heal 後の状態を再プローブして反映（成功していれば healthy に戻る）
            let host = host_alias.to_string();
            let post = tokio::task::spawn_blocking(move || {
                agent_socket::probe_agent_forward(&host, &|msg| eprintln!("[ccc] {msg}"))
            })
            .await
            .unwrap_or(ForwardHealth::Unreachable);
            self.forward_status.insert(host_alias.to_string(), post);
            eprintln!(
                "[ccc] [monitor] {host_alias}: 自動 heal 後の状態: {}",
                post.as_slug()
            );
            // 遷移が確定的にあるので必ず emit
            self.emit_forward_status(host_alias, post, true);
            return;
        }

        if transitioned {
            eprintln!(
                "[ccc] [monitor] {host_alias}: gpg forward 状態遷移 → {}",
                health.as_slug()
            );
            self.emit_forward_status(host_alias, health, false);
        }
    }

    /// gpg forward 状態を Tauri event で通知する。
    fn emit_forward_status(
        &self,
        host_alias: &str,
        health: ccc_sshkit::agent_socket::ForwardHealth,
        auto_heal_triggered: bool,
    ) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        super::notify::emit_gpg_forward_status(
            &self.app_handle.get().cloned(),
            super::types::GpgForwardStatusPayload {
                host_alias: host_alias.to_string(),
                status: health.as_slug().to_string(),
                checked_at,
                auto_heal_triggered,
            },
        );
    }

    /// 現在 PTY が生きているリモートインスタンスの host_alias 一覧（重複排除済み）。
    /// port forward 台帳の定期世代チェック対象を決めるのに使う。
    pub fn active_remote_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .infos
            .iter()
            .filter(|e| self.instances.contains_key(e.key()))
            .filter_map(|e| e.value().host_alias.clone())
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// 60 秒ループから呼ぶネットワーク断耐性チェック（v0.13）。
    ///
    /// 死活プローブは gpg 世代ゲートの**外**で毎サイクル実行する。half-open
    /// master は `-O check` に応答し pid も不変なため、ゲートでは検知できない。
    ///
    /// - master 不在: 何もしない（意図しない自動接続を発生させない）
    /// - 健全: gpg forward チェック（ゲートが効くのでコストは -O check 程度）+ 台帳同期
    /// - half-open/wedged: 連続 2 サイクルで畳んで再確立 → gpg forward 修復
    pub async fn network_resilience_tick(&self, host_alias: &str) {
        use ccc_sshkit::liveness::{self, MasterLiveness};

        let stderr_log = |msg: &str| eprintln!("[ccc] {msg}");

        let liveness = {
            let host = host_alias.to_string();
            tokio::task::spawn_blocking(move || {
                liveness::probe_master(&host, liveness::DEFAULT_PROBE_TIMEOUT, &|msg| {
                    eprintln!("[ccc] {msg}")
                })
            })
            .await
            .unwrap_or(MasterLiveness::NoMaster)
        };

        let (strike_target_pid, wedged) = match liveness {
            MasterLiveness::NoMaster => {
                self.resilience_strikes.remove(host_alias);
                return;
            }
            MasterLiveness::Alive { .. } => {
                self.resilience_strikes.remove(host_alias);
                // gpg forward の状態プローブ（ゲートなし）→ broken なら無条件で自動 heal。
                // 台帳の世代交代リプレイもここで巻き込む（旧来と同じ位置づけ）。
                self.probe_and_maybe_heal(host_alias).await;
                let host = host_alias.to_string();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::forwards::sync_ledger(&host);
                })
                .await;
                return;
            }
            MasterLiveness::SessionRefused { .. } => {
                // サーバは応答している（MaxSessions 超過等）。生きたセッションを
                // 巻き込まないため畳まず、警告に留める
                stderr_log(&format!(
                    "[resilience] {host_alias}: mux session を開けないが応答はある（畳まない）"
                ));
                self.resilience_strikes.remove(host_alias);
                return;
            }
            MasterLiveness::HalfOpen { pid } => (pid, false),
            MasterLiveness::Wedged => (None, true),
        };

        let strikes = {
            let mut entry = self
                .resilience_strikes
                .entry(host_alias.to_string())
                .or_insert(0);
            *entry += 1;
            *entry
        };
        if strikes < 2 {
            stderr_log(&format!(
                "[resilience] {host_alias}: master 不調を検知（{}）。次サイクルも継続なら復旧します",
                if wedged { "wedged" } else { "half-open" }
            ));
            return;
        }

        stderr_log(&format!(
            "[resilience] {host_alias}: master 不調が {strikes} サイクル継続 → 自動復旧を開始"
        ));

        let user_cm = {
            let host = host_alias.to_string();
            tokio::task::spawn_blocking(move || {
                crate::ssh_config::user_has_control_master(&host).unwrap_or(false)
            })
            .await
            .unwrap_or(false)
        };

        let recovered = if user_cm {
            // ユーザー CM 尊重モード: 畳み + `ssh -N -f` 再確立まで sshkit に任せる
            let host = host_alias.to_string();
            tokio::task::spawn_blocking(move || {
                liveness::recover_half_open(&host, true, &|msg| eprintln!("[ccc] {msg}"))
            })
            .await
            .unwrap_or(false)
        } else {
            // ccc 専用 master: 畳んでから既存の確立パス（hook 逆転送付き）で立て直す
            {
                let host = host_alias.to_string();
                let pid = strike_target_pid
                    .or_else(|| ccc_sshkit::agent_socket::last_healthy_pid(host_alias));
                let _ = tokio::task::spawn_blocking(move || {
                    liveness::teardown_master(&host, pid, &|msg| eprintln!("[ccc] {msg}"))
                })
                .await;
            }
            match self.ensure_remote_master(host_alias, None).await {
                Ok(_) => true,
                Err(e) => {
                    stderr_log(&format!(
                        "[resilience] {host_alias}: master 再確立失敗（次サイクルで再試行）: {e}"
                    ));
                    false
                }
            }
        };

        if recovered {
            self.resilience_strikes.remove(host_alias);
            let host = host_alias.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                crate::agent_socket::ensure_agent_forward(&host, None);
                crate::forwards::sync_ledger(&host);
            })
            .await;
            stderr_log(&format!("[resilience] {host_alias}: 自動復旧完了"));
        }
    }

    /// ccc 自身が立てた ControlMaster をすべて閉じる。
    /// アプリ終了時に lib.rs から呼ぶ前提（同期）。
    pub fn shutdown_ssh_masters(&self) {
        let aliases: Vec<String> = self
            .ssh_masters_owned
            .iter()
            .map(|e| e.key().clone())
            .collect();
        for alias in aliases {
            let _ = ssh_master_exit(&alias);
            self.ssh_masters_owned.remove(&alias);
        }
    }

    /// リモートホストに `~/.ccc/bin/ccc-hook.sh` を最新の endpoint/token で配信する。
    ///
    /// ccc 再起動でポートとトークンが変わるため、create_remote 時だけでなく
    /// reconnect / restore 経由でも呼ぶ必要がある（古い endpoint のまま claude code
    /// が起動していると hook が全部 401/接続拒否で落ちる）。失敗してもインスタンス
    /// 起動自体は止めず、状態表示の劣化として警告ログだけ残す。
    fn push_remote_wrapper(&self, host_alias: &str, log_path: Option<&Path>) {
        let Some((endpoint, token)) = self.hook_credentials() else {
            eprintln!(
                "[ccc] '{host_alias}': hook credentials が未確定のため wrapper 配信をスキップ"
            );
            return;
        };
        let creds = crate::hook_setup::wrapper::HookCredentials {
            endpoint: &endpoint,
            token: &token,
        };
        let t = std::time::Instant::now();
        match crate::hook_setup::wrapper::install_remote(host_alias, &creds) {
            Ok(()) => debug_log::append(
                log_path,
                &format!(
                    "[wrapper] {host_alias}: ccc-hook.sh 配信完了 (+{}ms)",
                    t.elapsed().as_millis()
                ),
            ),
            Err(e) => {
                eprintln!("[ccc] '{host_alias}' に hook wrapper 配信失敗: {e}");
                debug_log::append(
                    log_path,
                    &format!("[wrapper] {host_alias}: ccc-hook.sh 配信失敗: {e}"),
                );
            }
        }
    }

    /// HookReceiver から渡された hook event を `InstanceInfo` に反映し、
    /// archive サービスへも転送する（events 記録・SessionStart メタ・ローカル取込）。
    pub fn apply_hook(&self, received: ReceivedHook) {
        // ウォッチドッグの無音判定用。hook が届いている限り補正は走らない。
        self.last_hook_at
            .insert(received.instance_id.clone(), Instant::now());

        // 順序逆転した古いイベントは状態反映だけスキップする
        // （archive への記録は履歴データなので continue する）。
        let stale = hook_dispatch::is_stale_event(
            &self.last_hook_sent_at,
            &received.instance_id,
            received.sent_at_us,
        );
        if stale {
            eprintln!(
                "[ccc] 順序逆転した hook を状態反映からスキップ: instance={} event={}",
                received.instance_id,
                received.kind.name()
            );
        } else {
            hook_dispatch::apply(
                &self.infos,
                &self.app_handle.get().cloned(),
                &received.instance_id,
                received.kind,
                &received.payload,
            );
            // hook が状態を再主張したので、画面補正の控えはクリアする
            self.screen_set.remove(&received.instance_id);
        }

        if let Some(archive) = self.archive.get() {
            let (meta, is_local) = match self.infos.get(&received.instance_id) {
                Some(info) => {
                    let is_local = matches!(info.kind, InstanceKind::Local);
                    let kind = if is_local { "local" } else { "remote" };
                    (
                        ccc_archive::InstanceMeta {
                            instance_id: received.instance_id.clone(),
                            name: Some(info.name.clone()),
                            kind: Some(kind.to_string()),
                            host_alias: info.host_alias.clone(),
                            agent_profile: Some(info.agent_profile.clone()),
                            directory: info.directory.clone(),
                        },
                        is_local,
                    )
                }
                None => (
                    ccc_archive::InstanceMeta {
                        instance_id: received.instance_id.clone(),
                        ..Default::default()
                    },
                    false,
                ),
            };
            // 境界トリガ（狙い撃ち pull）: リモートの Stop / SessionEnd で、その
            // transcript（リモート実パス）だけを rsync pull → 取り込む。host_alias /
            // agent_profile は meta から拾う（record_hook へ move する前に控える）。
            let pull_target = (!is_local
                && matches!(
                    received.kind,
                    HookEventKind::Stop | HookEventKind::SessionEnd
                ))
            .then(|| {
                meta.host_alias.clone().and_then(|host| {
                    received
                        .payload
                        .get("transcript_path")
                        .and_then(|v| v.as_str())
                        .map(|tp| {
                            (
                                host,
                                meta.agent_profile
                                    .clone()
                                    .unwrap_or_else(|| "default".into()),
                                tp.to_string(),
                            )
                        })
                })
            })
            .flatten();

            archive.record_hook(crate::archive_service::HookJob {
                hook_event: received.kind.name().to_string(),
                payload: received.payload.clone(),
                meta,
                is_local,
            });

            if let Some((host_alias, profile, transcript_path)) = pull_target {
                archive.record_pull(crate::archive_service::PullRequest {
                    host_alias,
                    profile,
                    transcript_path: Some(transcript_path),
                });
            }
        }
    }

    /// `RelayContext` をインスタンスIDから組み立てる。
    /// 5箇所以上で同じ構築をしていたためヘルパー化。
    /// relay 1本につき 1 回呼ばれるため、シャドウスクリーンの生成・登録もここで行う
    /// （再接続時は新しいモニタで置き換え、古い画面状態を引きずらない）。
    fn relay_context(&self, id: InstanceId) -> RelayContext {
        let (rows, cols) = self
            .infos
            .get(&id)
            .and_then(|i| i.last_size)
            .unwrap_or((24, 80));
        let monitor = Arc::new(Mutex::new(ScreenMonitor::new(rows, cols)));
        self.screen_monitors
            .insert(id.clone(), Arc::clone(&monitor));
        RelayContext {
            instance_id: id,
            infos: Arc::clone(&self.infos),
            app_handle: self.app_handle.get().cloned(),
            monitor,
        }
    }

    // ─── ローカルインスタンス ─────────────────────────────────────────────────

    pub fn create_local(
        &self,
        command: &str,
        directory: Option<&str>,
        _copy_auth_from_id: Option<&str>,
        agent_profile: Option<&str>,
        name: Option<&str>,
    ) -> anyhow::Result<InstanceId> {
        let id = uuid::Uuid::new_v4().to_string();
        let resolved_dir = resolve_local_directory(directory);
        let name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| derive_instance_name(None, &resolved_dir));
        let tmux_name = generate_tmux_session_name();
        let hash = instance_hash();
        let instance_dir = storage::instance_dir_for(&name, &hash)?;
        storage::ensure_instance_dir(&instance_dir)?;

        let profile = agent_profile.filter(|s| !s.is_empty()).unwrap_or("default");
        let claude_cfg = match agent_config::local_claude_config_dir(profile) {
            Ok(c) => c,
            Err(e) => {
                let _ = storage::delete_instance_dir(&instance_dir);
                return Err(e);
            }
        };
        let claude_cfg_str = claude_cfg.to_string_lossy().to_string();

        let info = InstanceInfo {
            id: id.clone(),
            kind: InstanceKind::Local,
            name: name.clone(),
            status: InstanceStatus::Running,
            host_alias: None,
            directory: Some(resolved_dir.clone()),
            command: Some(command.to_string()),
            tmux_session: Some(tmux_name.clone()),
            instance_hash: hash,
            instance_dir: instance_dir.clone(),
            agent_profile: profile.to_string(),
            status_message: None,
            pending_prompt: None,
            current_session_id: None,
            session_title: None,
            last_size: None,
            last_transcript_path: None,
        };
        if let Err(e) = storage::save_connection(&info) {
            let _ = storage::delete_instance_dir(&instance_dir);
            return Err(e);
        }

        let (bcast_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CHANNEL_SIZE);
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);

        let hook_env = HookEnv {
            instance_id: Some(&id),
        };

        let instance = match PtyInstance::spawn_local(
            command,
            &resolved_dir,
            &tmux_name,
            raw_tx,
            Some(&claude_cfg_str),
            &hook_env,
        ) {
            Ok(i) => i,
            Err(e) => {
                let _ = storage::delete_instance_dir(&instance_dir);
                return Err(e);
            }
        };

        relay::spawn_relay(
            self.relay_context(id.clone()),
            raw_rx,
            bcast_tx.clone(),
            false,
        );

        self.broadcasts.insert(id.clone(), bcast_tx);
        self.instances.insert(id.clone(), instance);
        self.infos.insert(id.clone(), info);

        Ok(id)
    }

    // ─── リモートインスタンス ─────────────────────────────────────────────────

    pub async fn create_remote(
        &self,
        host_alias: &str,
        command: &str,
        directory: Option<&str>,
        _copy_auth_from_id: Option<&str>,
        agent_profile: Option<&str>,
        name: Option<&str>,
    ) -> anyhow::Result<InstanceId> {
        let hosts = ssh_config::load()?;
        if !hosts.iter().any(|h| h.alias == host_alias) {
            anyhow::bail!("SSH host '{host_alias}' not found in ~/.ssh/config");
        }

        let id = uuid::Uuid::new_v4().to_string();
        let dir_str = directory.unwrap_or("");
        let name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| derive_instance_name(Some(host_alias), dir_str));
        let tmux_name = generate_tmux_session_name();
        let hash = instance_hash();
        let instance_dir = storage::instance_dir_for(&name, &hash)?;
        storage::ensure_instance_dir(&instance_dir)?;

        let log_path = Some(instance_dir.join(".debug.txt"));
        let profile = agent_profile.filter(|s| !s.is_empty()).unwrap_or("default");
        debug_log::append(log_path.as_deref(), &format!(
            "[create_remote] 開始: host={host_alias}, dir={dir_str:?}, profile={profile}, tmux={tmux_name}"
        ));

        let t = std::time::Instant::now();
        debug_log::append(
            log_path.as_deref(),
            "[create_remote] prepare_remote_claude_config 開始",
        );
        let remote_cfg = match agent_config::prepare_remote_claude_config(
            host_alias,
            profile,
            log_path.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => {
                debug_log::append(
                    log_path.as_deref(),
                    &format!("[create_remote] prepare_remote_claude_config 失敗: {e}"),
                );
                let _ = storage::delete_instance_dir(&instance_dir);
                return Err(e);
            }
        };
        debug_log::append(
            log_path.as_deref(),
            &format!(
                "[create_remote] prepare_remote_claude_config 完了 (+{}ms)",
                t.elapsed().as_millis()
            ),
        );

        // hook バイナリは prepare_remote_claude_config 内で配信済み。ここで
        // 最新の endpoint/token を埋め込んだ wrapper script を上書き push する。
        self.push_remote_wrapper(host_alias, log_path.as_deref());

        // ssh ControlMaster を確立（forward + health check）。これより前に wrapper を
        // push しておかないと health check の hook 呼び出しで古い endpoint/token を
        // 叩いてしまう。
        let ccc_master = match self
            .ensure_remote_master(host_alias, log_path.as_deref())
            .await
        {
            Ok(v) => v,
            Err(e) => {
                debug_log::append(
                    log_path.as_deref(),
                    &format!("[create_remote] ensure_remote_master 失敗: {e}"),
                );
                let _ = storage::delete_instance_dir(&instance_dir);
                anyhow::bail!("ssh ControlMaster の確立に失敗: {e}");
            }
        };

        // gpg agent forward の健全性チェック＋自動修復。エージェントコマンドが
        // gpg を要求する場合（secret-tool 等）の即死を起動前に防ぐ。
        // 修復失敗でも起動は続行する（即死は pane-died 検知が出力ごと保全する）。
        {
            let alias = host_alias.to_string();
            let lp = log_path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::agent_socket::ensure_agent_forward(&alias, lp.as_deref())
            })
            .await;
        }

        let info = InstanceInfo {
            id: id.clone(),
            kind: InstanceKind::Remote,
            name: name.clone(),
            status: InstanceStatus::Connecting,
            host_alias: Some(host_alias.to_string()),
            directory: if dir_str.is_empty() {
                None
            } else {
                Some(dir_str.to_string())
            },
            command: Some(command.to_string()),
            tmux_session: Some(tmux_name.clone()),
            instance_hash: hash.clone(),
            instance_dir: instance_dir.clone(),
            agent_profile: profile.to_string(),
            status_message: None,
            pending_prompt: None,
            current_session_id: None,
            session_title: None,
            last_size: None,
            last_transcript_path: None,
        };
        if let Err(e) = storage::save_connection(&info) {
            let _ = storage::delete_instance_dir(&instance_dir);
            return Err(e);
        }
        self.infos.insert(id.clone(), info);

        let (bcast_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CHANNEL_SIZE);
        self.broadcasts.insert(id.clone(), bcast_tx.clone());

        let ssh_args = ssh_config::build_slave_ssh_args(host_alias, ccc_master, self.hook_port())?;
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);
        debug_log::append(
            log_path.as_deref(),
            &format!("[create_remote] spawn_remote 開始: ssh_args={ssh_args:?}"),
        );
        let hook_env = HookEnv {
            instance_id: Some(&id),
        };
        let (instance, ready_rx) = match PtyInstance::spawn_remote(
            &ssh_args,
            command,
            directory,
            &tmux_name,
            raw_tx,
            Some(&remote_cfg),
            &hook_env,
            log_path.clone(),
        ) {
            Ok(p) => p,
            Err(e) => {
                self.cleanup_failed(&id, &instance_dir);
                return Err(e);
            }
        };

        relay::spawn_relay(
            self.relay_context(id.clone()),
            raw_rx,
            bcast_tx.clone(),
            true,
        );

        match await_ssh_ready(ready_rx, log_path.as_deref(), "create_remote").await {
            Ok(()) => {
                self.instances.insert(id.clone(), instance);
                self.update_status(&id, InstanceStatus::Running);
                Ok(id)
            }
            Err(msg) => {
                self.cleanup_failed(&id, &instance_dir);
                anyhow::bail!("SSH 接続に失敗しました: {msg}")
            }
        }
    }

    fn cleanup_failed(&self, id: &str, instance_dir: &Path) {
        self.infos.remove(id);
        self.broadcasts.remove(id);
        let _ = storage::delete_instance_dir(instance_dir);
    }

    /// 切断済みインスタンスを同じIDで再接続する
    pub async fn reconnect(&self, id: &str) -> anyhow::Result<()> {
        let info = {
            let guard = self
                .infos
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("instance not found: {id}"))?;
            guard.clone()
        };

        let tmux_session = info
            .tmux_session
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tmux session name not found"))?;

        self.update_status(id, InstanceStatus::Connecting);

        match info.kind {
            InstanceKind::Local => {
                let result = self.start_local_reattach(id, &tmux_session).await;
                if result.is_err() {
                    // フロントは status 補償をしない（emit が単一の真実源）ため、
                    // 失敗時はここで Disconnected に落とす。
                    self.update_status(id, InstanceStatus::Disconnected);
                }
                result
            }
            InstanceKind::Remote => {
                let host_alias = info
                    .host_alias
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("host_alias not found for remote instance"))?;

                let log_path = Some(info.instance_dir.join(".debug.txt"));
                debug_log::append(
                    log_path.as_deref(),
                    &format!("[reconnect] 再接続開始: host={host_alias}, tmux={tmux_session}"),
                );

                // ccc 再起動を跨いだ場合に endpoint/token が変わっているので、
                // reattach 前に wrapper script を最新内容で上書き push しておく。
                self.push_remote_wrapper(&host_alias, log_path.as_deref());

                // wrapper push の直後に ControlMaster + reverse forward + health check を確立する。
                let ccc_master = match self
                    .ensure_remote_master(&host_alias, log_path.as_deref())
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        debug_log::append(
                            log_path.as_deref(),
                            &format!("[reconnect] ensure_remote_master 失敗: {e}"),
                        );
                        self.update_status(id, InstanceStatus::Disconnected);
                        anyhow::bail!("ssh ControlMaster の確立に失敗: {e}");
                    }
                };

                // gpg agent forward の健全性チェック＋自動修復（create_remote と同様）。
                {
                    let alias = host_alias.clone();
                    let lp = log_path.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::agent_socket::ensure_agent_forward(&alias, lp.as_deref())
                    })
                    .await;
                }

                let ssh_args =
                    ssh_config::build_slave_ssh_args(&host_alias, ccc_master, self.hook_port())?;
                debug_log::append(
                    log_path.as_deref(),
                    &format!("[reconnect] ssh_args={ssh_args:?}"),
                );

                let bcast_tx = {
                    let guard = self
                        .broadcasts
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("broadcast channel not found: {id}"))?;
                    guard.clone()
                };

                let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);
                let (instance, ready_rx) = PtyInstance::reattach_remote(
                    &ssh_args,
                    &tmux_session,
                    raw_tx,
                    log_path.clone(),
                )?;

                relay::spawn_relay(
                    self.relay_context(id.to_string()),
                    raw_rx,
                    bcast_tx.clone(),
                    true,
                );

                match await_ssh_ready(ready_rx, log_path.as_deref(), "reconnect").await {
                    Ok(()) => {
                        // reattach 直後の PTY は default 80x24。直近サイズが分かっていれば
                        // 即座に SIGWINCH を発火させて tmux クライアントを再描画させる。
                        if let Some((rows, cols)) = info.last_size {
                            if let Err(e) = instance.resize(rows, cols).await {
                                debug_log::append(
                                    log_path.as_deref(),
                                    &format!("[reconnect] last_size 適用失敗 ({rows}x{cols}): {e}"),
                                );
                            }
                        }
                        self.instances.insert(id.to_string(), instance);
                        self.update_status(id, InstanceStatus::Running);
                        Ok(())
                    }
                    Err(msg) => {
                        self.update_status(id, InstanceStatus::Disconnected);
                        anyhow::bail!("SSH 再接続に失敗しました: {msg}")
                    }
                }
            }
        }
    }

    /// インスタンスのステータスを更新し、永続化とフロント通知を行う。
    /// バックエンドを状態の単一の真実源とするため、status 変更は必ず emit を伴う
    /// （フロント側での補償的な status 上書きを不要にする）。
    fn update_status(&self, id: &str, status: InstanceStatus) {
        // 切断/終了の境界では、リモートの取りこぼし（hook で観測できなかった孤児
        // ファイル・メモリ）を全面 sweep pull で回収する（破棄前の最後のチャンス）。
        if matches!(
            status,
            InstanceStatus::Disconnected | InstanceStatus::Terminated
        ) {
            self.sweep_pull_remote(id);
        }
        update_status_and_notify(&self.infos, &self.app_handle.get().cloned(), id, status);
    }

    /// リモートインスタンスの `projects/` 全体を 1 回 sweep pull する（切断/終了時）。
    /// ローカル・archive 無効時・host_alias 無しは何もしない。
    fn sweep_pull_remote(&self, id: &str) {
        let Some(archive) = self.archive.get() else {
            return;
        };
        let Some(info) = self.infos.get(id) else {
            return;
        };
        if !matches!(info.kind, InstanceKind::Remote) {
            return;
        }
        if let Some(host_alias) = info.host_alias.clone() {
            archive.record_pull(crate::archive_service::PullRequest {
                host_alias,
                profile: info.agent_profile.clone(),
                transcript_path: None,
            });
        }
    }

    /// 切断済み / 終了済みインスタンスを同じ設定で作り直す。
    ///
    /// 「リモートの tmux セッションが消えていて reconnect では復活不能」
    /// 「ローカルで tmux server が落ちている」等の状況でユーザに優しい
    /// リカバリパスを提供する。新しいインスタンスが立ち上がってから古い
    /// インスタンスを close する（途中失敗時に古いインスタンスを残せる）。
    pub async fn recreate(&self, id: &str) -> anyhow::Result<InstanceId> {
        let info = self
            .infos
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("instance not found: {id}"))?
            .clone();

        let command = info.command.as_deref().unwrap_or("");
        let directory = info.directory.as_deref();
        let agent_profile = Some(info.agent_profile.as_str());
        let name = Some(info.name.as_str());

        let new_id = match info.kind {
            InstanceKind::Local => {
                self.create_local(command, directory, None, agent_profile, name)?
            }
            InstanceKind::Remote => {
                let host_alias = info.host_alias.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("リモートインスタンスに host_alias が設定されていません")
                })?;
                self.create_remote(host_alias, command, directory, None, agent_profile, name)
                    .await?
            }
        };

        // 新規作成に成功した時点で古い側を片付ける。
        // close は tmux セッション自体には触れないため、もし残っていても無害。
        self.close(id);

        Ok(new_id)
    }

    // ─── 保存・復元 ───────────────────────────────────────────────────────────

    /// 終了時のフラッシュ。新しいストレージは逐次保存されているため、
    /// 各インスタンスの最終状態のみ書き戻す。
    pub fn save_state(&self) {
        for entry in self.infos.iter() {
            let _ = storage::save_connection(entry.value());
        }
    }

    /// `~/.ccc/instances/` を走査し、保存済みインスタンスを tmux reattach で復元する。
    pub async fn restore(&self) {
        let dirs = match storage::list_instance_dirs() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ccc] インスタンスディレクトリ一覧の取得に失敗: {e}");
                return;
            }
        };

        for dir in dirs {
            // connection.json が無いディレクトリは create_* 中の早期失敗で残った
            // 孤児なので削除する。Parse エラー (壊れた JSON 等) は誤判定を避けて
            // 残し、ログだけ出す。
            if !storage::connection_path(&dir).exists() {
                eprintln!(
                    "[ccc] connection.json が無い不完全なインスタンスディレクトリを削除: {}",
                    dir.display()
                );
                let _ = storage::delete_instance_dir(&dir);
                continue;
            }

            let mut info = match storage::load_connection(&dir) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[ccc] {dir:?} の connection.json 読み込みに失敗: {e}");
                    continue;
                }
            };
            // ファイルパス自体は load_connection で復元済みだが、保存後に
            // ディレクトリが移動された可能性に備え、現在の dir で上書きする。
            info.instance_dir = dir.clone();
            // session_title は永続化された値をそのまま表示する。
            // 起動直後の最初の hook 受信 → archive ingest が走ると、
            // archive_service::refresh_session_title が DB 由来の最新値で上書きする。

            let tmux_session = match &info.tmux_session {
                Some(s) => s.clone(),
                None => continue,
            };

            match info.kind {
                InstanceKind::Local => {
                    // tmux セッションが存在しない場合（ccc 再起動などでサーバが落ちている）は
                    // reattach を試みず Terminated として一覧に残す。
                    // ここで reattach に進むと、PTY 内の shell が `tmux attach` で即終了し、
                    // relay が `instance-terminated` を emit → フロントが close →
                    // インスタンスディレクトリが削除される、という事故を引き起こす。
                    if !local_tmux_session_exists(&tmux_session) {
                        eprintln!(
                            "[ccc] tmux セッション '{tmux_session}' が見つかりません。インスタンス {} を Terminated として保持します",
                            info.id
                        );
                        self.infos.insert(
                            info.id.clone(),
                            InstanceInfo {
                                status: InstanceStatus::Terminated,
                                ..info
                            },
                        );
                        continue;
                    }
                    if let Err(e) = self.start_local_reattach(&info.id, &tmux_session).await {
                        eprintln!("[ccc] ローカルインスタンス {} の復元に失敗: {e}", info.id);
                        // 再アタッチに失敗しても一覧からは消さず、ユーザが手動 close するまで残す。
                        self.infos.insert(
                            info.id.clone(),
                            InstanceInfo {
                                status: InstanceStatus::Terminated,
                                ..info
                            },
                        );
                        continue;
                    }
                    self.infos.insert(
                        info.id.clone(),
                        InstanceInfo {
                            status: InstanceStatus::Running,
                            ..info
                        },
                    );
                }
                InstanceKind::Remote => {
                    let host_alias = match info.host_alias.clone() {
                        Some(s) => s,
                        None => continue,
                    };

                    let hook_port = match self.hook_port() {
                        Some(p) => p,
                        None => {
                            eprintln!(
                                "[ccc] hook_port 未確定のためリモートインスタンス {} の復元を Disconnected で保留",
                                info.id
                            );
                            self.infos.insert(
                                info.id.clone(),
                                InstanceInfo {
                                    status: InstanceStatus::Disconnected,
                                    ..info
                                },
                            );
                            continue;
                        }
                    };

                    let log_path = Some(info.instance_dir.join(".debug.txt"));
                    debug_log::append(
                        log_path.as_deref(),
                        &format!("[restore] 復元開始: host={host_alias}, tmux={tmux_session}"),
                    );

                    // 同期で確実に終わるのは「Connecting で infos 登録」「broadcasts 登録」だけ。
                    // ssh/scp 系（wrapper push・master 確立・scrollback 取得・reattach・ready 待ち）は
                    // 全て別タスクに逃がす。でないと unreachable な remote ホストの ssh タイムアウト分
                    // （数十秒）だけ restore_instances 全体がブロックされ、フロントに一覧が届かない。
                    self.infos.insert(
                        info.id.clone(),
                        InstanceInfo {
                            status: InstanceStatus::Connecting,
                            ..info.clone()
                        },
                    );

                    let (bcast_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CHANNEL_SIZE);
                    self.broadcasts.insert(info.id.clone(), bcast_tx.clone());

                    let scrollback_lines = crate::settings::load()
                        .map(|s| s.display.scrollback_lines)
                        .unwrap_or(0);

                    let relay_ctx = self.relay_context(info.id.clone());
                    let infos = Arc::clone(&self.infos);
                    let pending_scrollback = Arc::clone(&self.pending_scrollback);
                    let instances = Arc::clone(&self.instances);
                    let app_handle = self.app_handle.get().cloned();
                    let id_owned = info.id.clone();
                    let host_alias_owned = host_alias.clone();
                    let tmux_session_owned = tmux_session.clone();
                    let master_locks = Arc::clone(&self.ssh_master_locks);
                    let masters_owned = Arc::clone(&self.ssh_masters_owned);
                    // hook credentials は spawn 内で再取得すると OnceLock 経由で
                    // ヘルパー名衝突するので、ここでスナップショットして move する。
                    let hook_creds_snapshot = self.hook_credentials();

                    tauri::async_runtime::spawn(async move {
                        // 0) hook wrapper を最新内容で push (ccc 再起動で endpoint/token が
                        //    変わっているケースを救う)。ssh が同期 blocking なので
                        //    spawn_blocking に逃がす。失敗しても reattach は続行する。
                        if let Some((endpoint, token)) = hook_creds_snapshot {
                            let host_for_wrapper = host_alias_owned.clone();
                            let log_for_wrapper = log_path.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                let creds = crate::hook_setup::wrapper::HookCredentials {
                                    endpoint: &endpoint,
                                    token: &token,
                                };
                                let t = std::time::Instant::now();
                                match crate::hook_setup::wrapper::install_remote(
                                    &host_for_wrapper, &creds,
                                ) {
                                    Ok(()) => debug_log::append(
                                        log_for_wrapper.as_deref(),
                                        &format!(
                                            "[wrapper] {host_for_wrapper}: ccc-hook.sh 配信完了 (+{}ms)",
                                            t.elapsed().as_millis()
                                        ),
                                    ),
                                    Err(e) => debug_log::append(
                                        log_for_wrapper.as_deref(),
                                        &format!(
                                            "[wrapper] {host_for_wrapper}: ccc-hook.sh 配信失敗: {e}"
                                        ),
                                    ),
                                }
                            })
                            .await;
                        }

                        // 1) ControlMaster 確立（host_alias 単位で直列化）
                        let ccc_master = match ensure_remote_master_impl(
                            &host_alias_owned,
                            hook_port,
                            &master_locks,
                            &masters_owned,
                            log_path.as_deref(),
                        )
                        .await
                        {
                            Ok(v) => v,
                            Err(e) => {
                                debug_log::append(
                                    log_path.as_deref(),
                                    &format!("[restore] ensure_remote_master 失敗: {e}"),
                                );
                                eprintln!(
                                    "[ccc] リモートインスタンス {id_owned} の復元失敗（master 確立不可）: {e}"
                                );
                                update_status_and_notify(
                                    &infos,
                                    &app_handle,
                                    &id_owned,
                                    InstanceStatus::Disconnected,
                                );
                                return;
                            }
                        };

                        let ssh_args = match ssh_config::build_slave_ssh_args(
                            &host_alias_owned,
                            ccc_master,
                            Some(hook_port),
                        ) {
                            Ok(a) => a,
                            Err(e) => {
                                debug_log::append(
                                    log_path.as_deref(),
                                    &format!("[restore] build_slave_ssh_args 失敗: {e}"),
                                );
                                update_status_and_notify(
                                    &infos,
                                    &app_handle,
                                    &id_owned,
                                    InstanceStatus::Disconnected,
                                );
                                return;
                            }
                        };
                        debug_log::append(
                            log_path.as_deref(),
                            &format!("[restore] ssh_args={ssh_args:?}"),
                        );

                        // 2) scrollback 取得 (ssh が同期 blocking なので spawn_blocking)
                        let host_for_capture = host_alias_owned.clone();
                        let tmux_for_capture = tmux_session_owned.clone();
                        let capture = tokio::task::spawn_blocking(move || {
                            capture_tmux_scrollback(
                                ScrollbackTarget::Remote {
                                    host_alias: &host_for_capture,
                                    tmux_session: &tmux_for_capture,
                                },
                                scrollback_lines,
                            )
                        })
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                        if !capture.is_empty() {
                            pending_scrollback.insert(id_owned.clone(), capture);
                        }

                        // 3) PTY (ssh 起動 + tmux attach) を立ち上げ
                        let (raw_tx, raw_rx) =
                            tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);
                        match PtyInstance::reattach_remote(
                            &ssh_args,
                            &tmux_session_owned,
                            raw_tx,
                            log_path.clone(),
                        ) {
                            Ok((instance, ready_rx)) => {
                                relay::spawn_relay(relay_ctx, raw_rx, bcast_tx.clone(), true);

                                let new_status = match await_ssh_ready(
                                    ready_rx,
                                    log_path.as_deref(),
                                    "restore",
                                )
                                .await
                                {
                                    Ok(()) => {
                                        // reattach 直後の PTY は default 80x24。reconnect と同様、
                                        // last_size が残っていれば即座に SIGWINCH を発火させる。
                                        // last_size は現状永続化対象外で初回起動時は None だが、
                                        // 永続化を導入した際に効くよう構造を揃えておく。
                                        let last_size =
                                            infos.get(&id_owned).and_then(|i| i.last_size);
                                        if let Some((rows, cols)) = last_size {
                                            if let Err(e) = instance.resize(rows, cols).await {
                                                debug_log::append(
                                                    log_path.as_deref(),
                                                    &format!("[restore] last_size 適用失敗 ({rows}x{cols}): {e}"),
                                                );
                                            }
                                        }
                                        InstanceStatus::Running
                                    }
                                    Err(_) => {
                                        eprintln!(
                                            "[ccc] リモートインスタンス {id_owned} の復元に失敗: SSH 接続エラー"
                                        );
                                        InstanceStatus::Disconnected
                                    }
                                };
                                instances.insert(id_owned.clone(), instance);
                                update_status_and_notify(
                                    &infos,
                                    &app_handle,
                                    &id_owned,
                                    new_status,
                                );
                            }
                            Err(e) => {
                                debug_log::append(
                                    log_path.as_deref(),
                                    &format!("[restore] reattach_remote 起動失敗: {e}"),
                                );
                                eprintln!(
                                    "[ccc] リモートインスタンス {id_owned} の復元に失敗: {e}"
                                );
                                update_status_and_notify(
                                    &infos,
                                    &app_handle,
                                    &id_owned,
                                    InstanceStatus::Disconnected,
                                );
                            }
                        }
                    });
                }
            }
        }

        // 起動時バックフィル: hook が届かなかった期間（ccc 未起動中など）に進んだ
        // リモートセッションを回収するため、復元した全リモートインスタンスへ
        // 全面 sweep pull を 1 回投げる。rsync は別スレッドで走り、復元をブロックしない。
        let remote_ids: Vec<String> = self
            .infos
            .iter()
            .filter(|e| matches!(e.value().kind, InstanceKind::Remote))
            .map(|e| e.key().clone())
            .collect();
        for id in remote_ids {
            self.sweep_pull_remote(&id);
        }

        // 復元完了後の一覧はコマンドの戻り値で返すため、event 発火はしない。
    }

    // ─── プライベートヘルパー ─────────────────────────────────────────────────

    /// ローカル tmux セッションに再アタッチする。
    async fn start_local_reattach(&self, id: &str, tmux_session: &str) -> anyhow::Result<()> {
        let scrollback_lines = crate::settings::load()
            .map(|s| s.display.scrollback_lines)
            .unwrap_or(0);

        // tmux capture-pane でスクロールバックを取得して pending_scrollback に格納。
        let capture =
            capture_tmux_scrollback(ScrollbackTarget::Local { tmux_session }, scrollback_lines)
                .unwrap_or_default();
        if !capture.is_empty() {
            self.pending_scrollback.insert(id.to_string(), capture);
        }

        let (bcast_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CHANNEL_SIZE);
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);

        let instance = PtyInstance::reattach_local(tmux_session, raw_tx)?;

        // reattach 直後の PTY は default 80x24。last_size が残っていれば即時 SIGWINCH を
        // 発火して tmux pane を正しいサイズに戻す。フロントの fit() を待たないと
        // 描画の整合が取れないため。
        if let Some((rows, cols)) = self.infos.get(id).and_then(|i| i.last_size) {
            if let Err(e) = instance.resize(rows, cols).await {
                eprintln!("[ccc] {id}: reattach_local 後の last_size 適用に失敗 ({rows}x{cols}): {e}");
            }
        }

        relay::spawn_relay(
            self.relay_context(id.to_string()),
            raw_rx,
            bcast_tx.clone(),
            false,
        );

        self.broadcasts.insert(id.to_string(), bcast_tx);
        self.instances.insert(id.to_string(), instance);
        self.update_status(id, InstanceStatus::Running);

        Ok(())
    }

    // ─── 共通操作 ─────────────────────────────────────────────────────────────

    pub fn subscribe(&self, id: InstanceId, channel: Channel<OutputPayload>) -> anyhow::Result<()> {
        if let Some((_, data)) = self.pending_scrollback.remove(&id) {
            let _ = channel.send(OutputPayload {
                instance_id: id.clone(),
                data,
            });
        }

        let bcast_tx = self
            .broadcasts
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("instance not found: {id}"))?;

        let mut rx = bcast_tx.subscribe();
        let iid = id.clone();

        self.cancel_txs.remove(&id);

        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.cancel_txs.insert(id.clone(), cancel_tx);

        tauri::async_runtime::spawn(async move {
            tokio::pin!(cancel_rx);
            loop {
                tokio::select! {
                    _ = &mut cancel_rx => break,
                    result = rx.recv() => {
                        match result {
                            Ok(data) => {
                                if channel
                                    .send(OutputPayload {
                                        instance_id: iid.clone(),
                                        data,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn write(&self, id: &str, data: &[u8]) -> anyhow::Result<()> {
        self.instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("instance not found: {id}"))?
            .value()
            .write(data)
            .await?;
        // 単独 Esc = 割り込みの可能性。矢印キー等のエスケープシーケンスは
        // `0x1b [ A` のように複数バイトで届くため単独バイト判定で区別できる。
        // Claude Code は割り込み停止時に Stop hook を発火しないため、
        // ウォッチドッグが transcript を確認して busy 固着を補正する。
        if data == [0x1b] {
            watchdog::on_escape_input(self.watchdog_ctx(), id);
        }
        Ok(())
    }

    /// ウォッチドッグへ渡すハンドル一式を組み立てる。
    fn watchdog_ctx(&self) -> watchdog::WatchdogCtx {
        watchdog::WatchdogCtx {
            infos: Arc::clone(&self.infos),
            app_handle: self.app_handle.get().cloned(),
            last_hook_at: Arc::clone(&self.last_hook_at),
            suspects: Arc::clone(&self.watchdog_suspects),
        }
    }

    /// シャドウスクリーンの評価タスクを起動する（setup で1回だけ呼ぶ）。
    /// EVAL_INTERVAL ごとに全モニタを巡回し、シグナルを評価して状態補正にかける。
    pub fn spawn_screen_evaluator(&self) {
        let monitors = Arc::clone(&self.screen_monitors);
        let ctx = screen_monitor::FusionCtx {
            infos: Arc::clone(&self.infos),
            app_handle: self.app_handle.get().cloned(),
            last_hook_at: Arc::clone(&self.last_hook_at),
            screen_set: Arc::clone(&self.screen_set),
        };
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(screen_monitor::EVAL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                // 評価対象 id を先に集めてから処理する（DashMap の iter を
                // 持ったまま infos をロックしない）
                let ids: Vec<InstanceId> = monitors.iter().map(|e| e.key().clone()).collect();
                for id in ids {
                    let Some(monitor) = monitors.get(&id).map(|m| Arc::clone(&m)) else {
                        continue;
                    };
                    let evaluated = monitor.lock().ok().and_then(|mut m| m.evaluate());
                    if let Some((signal, stable)) = evaluated {
                        screen_monitor::apply_correction(&ctx, &id, &signal, stable);
                    }
                }
            }
        });
    }

    /// ウォッチドッグの定期保険ポーリングを起動する（setup で1回だけ呼ぶ）。
    pub fn spawn_watchdog(&self) {
        let infos = Arc::clone(&self.infos);
        let app_handle = self.app_handle.get().cloned();
        let last_hook_at = Arc::clone(&self.last_hook_at);
        let suspects = Arc::clone(&self.watchdog_suspects);
        watchdog::spawn_periodic(move || watchdog::WatchdogCtx {
            infos: Arc::clone(&infos),
            app_handle: app_handle.clone(),
            last_hook_at: Arc::clone(&last_hook_at),
            suspects: Arc::clone(&suspects),
        });
    }

    pub async fn resize(&self, id: &str, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("instance not found: {id}"))?
            .value()
            .resize(rows, cols)
            .await?;
        if let Some(mut info_ref) = self.infos.get_mut(id) {
            // 同じサイズの繰り返し resize で disk I/O を増やさないため、変化時のみ save。
            // 永続化することで、ccc がクラッシュしても次回起動の reattach に効く。
            if info_ref.last_size != Some((rows, cols)) {
                info_ref.last_size = Some((rows, cols));
                let _ = storage::save_connection(&info_ref);
            }
        }
        // シャドウスクリーンにもサイズを転送（tmux の全再描画で内容は自己回復する）
        if let Some(monitor) = self.screen_monitors.get(id) {
            if let Ok(mut m) = monitor.lock() {
                m.resize(rows, cols);
            }
        }
        Ok(())
    }

    // ─── Terminal タブ（shell）操作 ───────────────────────────────────────────

    /// Terminal タブ用 PTY を初回起動 or 既存を再利用する。
    /// 冪等: 既に shell PTY が居れば何もしない。
    pub async fn ensure_shell_started(&self, id: &str) -> Result<(), String> {
        if self.shell_instances.contains_key(id) {
            eprintln!("[ccc] ensure_shell_started({id}): 既に起動済み → no-op");
            return Ok(());
        }

        let info = self
            .infos
            .get(id)
            .ok_or_else(|| format!("instance not found: {id}"))?
            .clone();

        let agent_session = info
            .tmux_session
            .clone()
            .ok_or_else(|| "tmux session name not found".to_string())?;
        let group_session = super::consts::shell_group_session_name(&agent_session);
        eprintln!(
            "[ccc] ensure_shell_started({id}): kind={:?}, agent_session={}, group_session={}",
            info.kind, agent_session, group_session
        );

        let (bcast_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CHANNEL_SIZE);
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_SIZE);

        match info.kind {
            InstanceKind::Local => {
                // Agent PTY が `tmux new-session` を発行し終える前にここに到達した場合、
                // shell の `new-window -t <agent_session>` は target 不在で失敗してしまう。
                // 起動直後のユーザー操作を救うため、最大 1 秒ほどポーリングして待つ。
                let mut waited = 0;
                for _ in 0..10 {
                    if local_tmux_session_exists(&agent_session) {
                        break;
                    }
                    waited += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                eprintln!(
                    "[ccc] ensure_shell_started({id}): local tmux session 確認 (waited {}ms, exists={})",
                    waited * 100,
                    local_tmux_session_exists(&agent_session)
                );

                let dir = info.directory.clone().unwrap_or_default();
                let instance = PtyInstance::spawn_local_shell(
                    &agent_session,
                    &group_session,
                    &dir,
                    raw_tx,
                )
                .map_err(|e| {
                    eprintln!("[ccc] ensure_shell_started({id}): spawn_local_shell 失敗: {e}");
                    format!("shell PTY 起動に失敗: {e}")
                })?;
                self.shell_broadcasts.insert(id.to_string(), bcast_tx.clone());
                self.spawn_shell_relay(id.to_string(), raw_rx, bcast_tx);
                self.shell_instances.insert(id.to_string(), instance);
                eprintln!("[ccc] ensure_shell_started({id}): 起動完了 (local)");
                Ok(())
            }
            InstanceKind::Remote => {
                let host_alias = info
                    .host_alias
                    .clone()
                    .ok_or_else(|| "host_alias not found for remote instance".to_string())?;
                let log_path = Some(info.instance_dir.join(".debug.txt"));
                let ccc_master = self
                    .ensure_remote_master(&host_alias, log_path.as_deref())
                    .await
                    .map_err(|e| {
                        eprintln!("[ccc] ensure_shell_started({id}): master 確立失敗: {e}");
                        format!("ssh ControlMaster の確立に失敗: {e}")
                    })?;
                let ssh_args = ssh_config::build_slave_ssh_args(
                    &host_alias,
                    ccc_master,
                    self.hook_port(),
                )
                .map_err(|e| format!("ssh 引数構築に失敗: {e}"))?;

                let (instance, ready_rx) = PtyInstance::spawn_remote_shell(
                    &ssh_args,
                    &agent_session,
                    &group_session,
                    info.directory.as_deref(),
                    raw_tx,
                    log_path.clone(),
                )
                .map_err(|e| {
                    eprintln!("[ccc] ensure_shell_started({id}): spawn_remote_shell 失敗: {e}");
                    format!("shell PTY 起動に失敗: {e}")
                })?;

                self.shell_broadcasts.insert(id.to_string(), bcast_tx.clone());
                self.spawn_shell_relay(id.to_string(), raw_rx, bcast_tx);

                await_ssh_ready(ready_rx, log_path.as_deref(), "ensure_shell").await?;
                self.shell_instances.insert(id.to_string(), instance);
                eprintln!("[ccc] ensure_shell_started({id}): 起動完了 (remote)");
                Ok(())
            }
        }
    }

    /// Shell 用 raw_rx → broadcast の最小リレー。Agent と異なり
    /// 画面状態検出 (ScreenMonitor) も disconnect emit も行わない。
    /// raw_rx が枯渇 (= shell PTY 終了) しても Terminal タブが grey out するだけで、
    /// 次回タブ表示時に `ensure_shell_started` を呼び直せば再起動される。
    fn spawn_shell_relay(
        &self,
        instance_id: InstanceId,
        mut raw_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
        bcast_tx: OutputBroadcast,
    ) {
        let shell_instances = Arc::clone(&self.shell_instances);
        let shell_broadcasts = Arc::clone(&self.shell_broadcasts);
        let shell_cancel_txs = Arc::clone(&self.shell_cancel_txs);
        let shell_pending = Arc::clone(&self.shell_pending_scrollback);
        tauri::async_runtime::spawn(async move {
            while let Some(data) = raw_rx.recv().await {
                let _ = bcast_tx.send(data);
            }
            // shell PTY が終了 (group session 消失や ssh 切断) したら、
            // 次回 ensure_shell_started で素直に作り直せるよう state をクリアする。
            shell_instances.remove(&instance_id);
            shell_broadcasts.remove(&instance_id);
            shell_cancel_txs.remove(&instance_id);
            shell_pending.remove(&instance_id);
        });
    }

    pub fn subscribe_shell(
        &self,
        id: InstanceId,
        channel: Channel<OutputPayload>,
    ) -> anyhow::Result<()> {
        if let Some((_, data)) = self.shell_pending_scrollback.remove(&id) {
            let _ = channel.send(OutputPayload {
                instance_id: id.clone(),
                data,
            });
        }

        let bcast_tx = self
            .shell_broadcasts
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("shell PTY not started: {id}"))?;
        let mut rx = bcast_tx.subscribe();
        let iid = id.clone();

        self.shell_cancel_txs.remove(&id);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.shell_cancel_txs.insert(id.clone(), cancel_tx);

        tauri::async_runtime::spawn(async move {
            tokio::pin!(cancel_rx);
            loop {
                tokio::select! {
                    _ = &mut cancel_rx => break,
                    result = rx.recv() => {
                        match result {
                            Ok(data) => {
                                if channel
                                    .send(OutputPayload {
                                        instance_id: iid.clone(),
                                        data,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn write_shell(&self, id: &str, data: &[u8]) -> anyhow::Result<()> {
        self.shell_instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("shell PTY not started: {id}"))?
            .value()
            .write(data)
            .await?;
        Ok(())
    }

    pub async fn resize_shell(&self, id: &str, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.shell_instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("shell PTY not started: {id}"))?
            .value()
            .resize(rows, cols)
            .await?;
        Ok(())
    }

    /// Terminal タブを閉じたとき用。Agent インスタンス自体は維持する。
    /// session-group メンバー（`<agent_session>-shell`）を kill して、
    /// 次回起動時にクリーンな状態から作り直せるようにする。
    pub fn close_shell(&self, id: &str) {
        self.shell_instances.remove(id);
        self.shell_broadcasts.remove(id);
        self.shell_cancel_txs.remove(id);
        self.shell_pending_scrollback.remove(id);

        // best-effort: group session を kill。Agent session は触らない。
        if let Some(info) = self.infos.get(id) {
            if let Some(agent_session) = info.tmux_session.clone() {
                let group_session = super::consts::shell_group_session_name(&agent_session);
                let kind = info.kind.clone();
                let host_alias = info.host_alias.clone();
                std::thread::spawn(move || {
                    kill_shell_group_session(kind, host_alias, &group_session);
                });
            }
        }
    }

    pub fn close(&self, id: &str) {
        let dir = self.infos.get(id).map(|i| i.instance_dir.clone());
        // Terminated（pane-died 等でコマンドが終了済み）のインスタンスを閉じるときは、
        // remain-on-exit で保全していた dead セッションを片付ける。
        // 生きているセッション（Running 等）は従来通り触らない（復元可能性を残す）。
        let dead_session = self.infos.get(id).and_then(|i| {
            matches!(i.status, InstanceStatus::Terminated)
                .then(|| {
                    i.tmux_session
                        .clone()
                        .map(|s| (i.kind.clone(), i.host_alias.clone(), s))
                })
                .flatten()
        });
        // Shell タブ用 PTY も同時に破棄。group session は agent session が kill されると
        // 連動して死ぬが、明示 kill しなくても害はないので state 削除のみ行う。
        self.shell_instances.remove(id);
        self.shell_broadcasts.remove(id);
        self.shell_cancel_txs.remove(id);
        self.shell_pending_scrollback.remove(id);

        self.instances.remove(id);
        self.broadcasts.remove(id);
        self.cancel_txs.remove(id);
        self.infos.remove(id);
        self.pending_scrollback.remove(id);
        self.screen_monitors.remove(id);
        self.screen_set.remove(id);
        self.last_hook_at.remove(id);
        self.last_hook_sent_at.remove(id);
        self.watchdog_suspects.remove(id);
        if let Some(dir) = dir {
            if !dir.as_os_str().is_empty() {
                let _ = storage::delete_instance_dir(&dir);
            }
        }
        if let Some((kind, host_alias, session)) = dead_session {
            // best effort。失敗してもセッションが残るだけ（従来と同じ状態）。
            std::thread::spawn(move || kill_dead_tmux_session(kind, host_alias, &session));
        }
    }

    pub fn list(&self) -> Vec<InstanceInfo> {
        self.infos.iter().map(|e| e.value().clone()).collect()
    }

    pub fn list_ssh_hosts(&self) -> anyhow::Result<Vec<SshHost>> {
        ssh_config::load()
    }
}

// ─── ヘルパー関数 ─────────────────────────────────────────────────────────────

/// `ensure_remote_master` の本体実装。`InstanceManager` を持たない `tauri::async_runtime::spawn`
/// 内からも `Arc<DashMap<...>>` をクローンして呼び出せるよう自由関数として切り出してある。
///
/// host_alias 単位の `AsyncMutex` で直列化し、同一ホスト宛の並行 reattach が来ても
/// master 起動は 1 度だけ走る。既存 master が `ssh -O check` で健在かつ health check に
/// 成功すれば再利用、いずれか失敗すれば `ssh -O exit` で破棄して立て直す。
pub(crate) async fn ensure_remote_master_impl(
    host_alias: &str,
    hook_port: u16,
    master_locks: &DashMap<String, Arc<AsyncMutex<()>>>,
    masters_owned: &DashMap<String, ()>,
    log_path: Option<&Path>,
) -> Result<bool, String> {
    // ユーザー側 ControlMaster が既設定なら ccc は手を出さない
    match ssh_config::user_has_control_master(host_alias) {
        Ok(true) => {
            debug_log::append(
                log_path,
                &format!(
                    "[master] {host_alias}: ユーザー側 ControlMaster を尊重（ccc 専用 master は立てない）"
                ),
            );
            return Ok(false);
        }
        Ok(false) => {}
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[master] {host_alias}: user_has_control_master 失敗: {e}"),
            );
            return Err(format!("ssh -G による設定解決に失敗: {e}"));
        }
    }

    let lock = master_locks
        .entry(host_alias.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    // 既存 master が健在なら再利用
    if ssh_master_check(host_alias) && health_check_via_master(host_alias).await.is_ok() {
        debug_log::append(
            log_path,
            &format!("[master] {host_alias}: 既存 master を再利用"),
        );
        // 既存が ccc 由来かは判別困難だが、終了時 cleanup の対象にする方が安全
        masters_owned.insert(host_alias.to_string(), ());
        return Ok(true);
    }

    // 旧 master が居れば破棄（失敗は無視）
    let _ = ssh_master_exit(host_alias);

    // 新 master が bind する前にリモート残骸 socket を ccc から明示的に unlink する。
    // sshd の StreamLocalBindUnlink 頼みだと旧 sshd forward 子プロセスが socket を握った
    // まま残っているケースで bind が沈黙失敗（あるいは ExitOnForwardFailure=yes で
    // ssh -M -N -f ごと非ゼロ終了）→ 「接続は張れているが gpg forward が切れる」障害
    // が再発する。掃除は best-effort（失敗しても続行、後段の check → repair が救う）。
    {
        let host = host_alias.to_string();
        let lp = log_path.map(|p| p.to_path_buf());
        let _ = tokio::task::spawn_blocking(move || {
            crate::agent_socket::cleanup_stale_remote_sockets(&host, lp.as_deref());
        })
        .await;
    }

    // 新規 master を起動
    let args = ssh_config::build_master_ssh_args(host_alias, hook_port)
        .map_err(|e| format!("master ssh 引数の構築に失敗: {e}"))?;
    let t = std::time::Instant::now();
    let host_for_spawn = host_alias.to_string();
    let args_for_spawn = args.clone();
    let started = tokio::task::spawn_blocking(move || {
        Command::new("ssh")
            .args(&args_for_spawn)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    if !started {
        debug_log::append(
            log_path,
            &format!(
                "[master] {host_for_spawn}: ssh -M -N -f 起動失敗 (+{}ms) args={args:?}",
                t.elapsed().as_millis()
            ),
        );
        return Err(
            "ssh ControlMaster の起動に失敗しました（forward bind 衝突またはSSH接続失敗）".into(),
        );
    }
    debug_log::append(
        log_path,
        &format!(
            "[master] {host_alias}: ssh -M -N -f 起動成功 (+{}ms)",
            t.elapsed().as_millis()
        ),
    );

    // health check（リモート上で `ccc-hook.sh --health-check` を実行）
    if let Err(e) = health_check_via_master(host_alias).await {
        debug_log::append(
            log_path,
            &format!("[master] {host_alias}: health check 失敗: {e}"),
        );
        let _ = ssh_master_exit(host_alias);
        return Err(format!("hook 疎通確認に失敗しました: {e}"));
    }
    debug_log::append(log_path, &format!("[master] {host_alias}: health check OK"));

    masters_owned.insert(host_alias.to_string(), ());
    Ok(true)
}

/// 既存 master 経由でリモートの `ccc-hook.sh --health-check` を 1 回叩く。
/// 必須 env のうち `CCC_INSTANCE_ID` だけ wrapper が注入しないため、ダミー値
/// `health` をコマンドライン側で付与する（server 側はログ表示にしか使わない）。
/// half-open master 上ではリモート実行が返らないため 15 秒で打ち切る
/// （タイムアウト = 失敗 → 呼び出し側の -O exit → 立て直しパスに乗る）。
async fn health_check_via_master(host_alias: &str) -> Result<(), String> {
    let host = host_alias.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        ccc_sshkit::exec::run_with_timeout(
            Command::new("ssh").args([
                "-o",
                &format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
                "-o",
                "BatchMode=yes",
                &host,
                "CCC_INSTANCE_ID=health \"$HOME/.ccc/bin/ccc-hook.sh\" --health-check",
            ]),
            std::time::Duration::from_secs(15),
        )
    })
    .await
    .map_err(|e| format!("health check の spawn_blocking 失敗: {e}"))?
    .map_err(|e| format!("ssh の起動に失敗: {e}"))?;
    if outcome.success() {
        Ok(())
    } else if outcome.timed_out {
        Err("ccc-hook.sh --health-check が 15 秒以内に応答しませんでした（master が half-open の可能性）".into())
    } else {
        Err(format!(
            "ccc-hook.sh --health-check が非ゼロ終了: {:?}",
            outcome.code
        ))
    }
}

/// `ssh -O check` で ccc 専用 ControlPath にぶら下がる master が健在か確認する。
/// 同期 blocking。exit code 0 = master あり、それ以外 = なし/不通/無応答。
fn ssh_master_check(host_alias: &str) -> bool {
    ccc_sshkit::exec::run_with_timeout(
        Command::new("ssh").args([
            "-O",
            "check",
            "-o",
            &format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
            host_alias,
        ]),
        crate::forwards::MUX_OP_TIMEOUT,
    )
    .map(|o| o.success())
    .unwrap_or(false)
}

/// Terminal タブの session-group メンバーを kill する（close_shell 時の後始末）。
/// agent_session 側には触らないので、Agent タブは生き続ける。
fn kill_shell_group_session(kind: InstanceKind, host_alias: Option<String>, session: &str) {
    let tmux_cmd = format!("{TMUX} kill-session -t '{session}'");
    let _ = match kind {
        InstanceKind::Local => Command::new("sh").args(["-c", &tmux_cmd]).status(),
        InstanceKind::Remote => {
            let Some(host) = host_alias else { return };
            Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=5",
                    &host,
                    &tmux_cmd,
                ])
                .status()
        }
    };
}

/// Terminated インスタンスの dead tmux セッションを削除する（close 時の後始末）。
/// セッションが既に存在しない場合や接続不可の場合は何もしない（best effort）。
fn kill_dead_tmux_session(kind: InstanceKind, host_alias: Option<String>, session: &str) {
    let tmux_cmd = format!("{TMUX} kill-session -t '{session}'");
    let status = match kind {
        InstanceKind::Local => Command::new("sh").args(["-c", &tmux_cmd]).status(),
        InstanceKind::Remote => {
            let Some(host) = host_alias else { return };
            Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=5",
                    &host,
                    &tmux_cmd,
                ])
                .status()
        }
    };
    if let Ok(s) = status {
        if !s.success() {
            eprintln!("[ccc] dead tmux session '{session}' の削除に失敗（無視）: {s}");
        }
    }
}

/// `ssh -O exit` で ccc 専用 ControlPath にぶら下がる master を閉じる。
/// 不在時もエラーになるが呼び出し側で無視してよい。
/// master が wedged で socket 無応答の場合もタイムアウトで制御を返す。
fn ssh_master_exit(host_alias: &str) -> bool {
    ccc_sshkit::exec::run_with_timeout(
        Command::new("ssh").args([
            "-O",
            "exit",
            "-o",
            &format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
            host_alias,
        ]),
        crate::forwards::MUX_OP_TIMEOUT,
    )
    .map(|o| o.success())
    .unwrap_or(false)
}

/// ローカルディレクトリを解決する。~ 展開、デフォルト HOME。
pub(crate) fn resolve_local_directory(directory: Option<&str>) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());

    match directory {
        Some(dir) if !dir.is_empty() => {
            if let Some(rest) = dir.strip_prefix("~/") {
                format!("{home}/{rest}")
            } else if dir == "~" {
                home
            } else {
                dir.to_string()
            }
        }
        _ => home,
    }
}

/// ccc 専用 tmux セッション名を生成する。
pub(crate) fn generate_tmux_session_name() -> String {
    let short = &uuid::Uuid::new_v4().to_string()[..8];
    format!("ccc-{short}")
}

/// インスタンスハッシュ (8文字)
pub(crate) fn instance_hash() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// インスタンス名をディレクトリベースで生成する。
pub(crate) fn derive_instance_name(host_alias: Option<&str>, directory: &str) -> String {
    let dirname = if directory.is_empty() {
        "~".to_string()
    } else {
        Path::new(directory)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "~".to_string())
    };

    match host_alias {
        Some(alias) => format!("{alias}:{dirname}"),
        None => dirname,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_local_directory ─────────────────────────────────────────

    #[test]
    fn test_resolve_local_directory_none() {
        let result = resolve_local_directory(None);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_resolve_local_directory_empty() {
        let result = resolve_local_directory(Some(""));
        assert!(!result.is_empty());
    }

    #[test]
    fn test_resolve_local_directory_tilde() {
        let result = resolve_local_directory(Some("~"));
        assert!(!result.contains('~'));
    }

    #[test]
    fn test_resolve_local_directory_tilde_path() {
        let result = resolve_local_directory(Some("~/projects/myapp"));
        assert!(result.ends_with("/projects/myapp"));
        assert!(!result.starts_with('~'));
    }

    #[test]
    fn test_resolve_local_directory_absolute() {
        let result = resolve_local_directory(Some("/tmp/work"));
        assert_eq!(result, "/tmp/work");
    }

    // ── derive_instance_name ────────────────────────────────────────────

    #[test]
    fn test_derive_name_local_with_path() {
        assert_eq!(derive_instance_name(None, "/Users/user/myapp"), "myapp");
    }

    #[test]
    fn test_derive_name_local_empty_dir() {
        assert_eq!(derive_instance_name(None, ""), "~");
    }

    #[test]
    fn test_derive_name_local_root() {
        assert_eq!(derive_instance_name(None, "/"), "~");
    }

    #[test]
    fn test_derive_name_remote_with_path() {
        assert_eq!(
            derive_instance_name(Some("dev-host"), "/home/user/myproject"),
            "dev-host:myproject"
        );
    }

    #[test]
    fn test_derive_name_remote_empty_dir() {
        assert_eq!(derive_instance_name(Some("dev-host"), ""), "dev-host:~");
    }

    // ── generate_tmux_session_name ──────────────────────────────────────

    #[test]
    fn test_tmux_session_name_format() {
        let name = generate_tmux_session_name();
        assert!(name.starts_with("ccc-"));
        assert_eq!(name.len(), 12);
    }

    #[test]
    fn test_tmux_session_name_unique() {
        let a = generate_tmux_session_name();
        let b = generate_tmux_session_name();
        assert_ne!(a, b);
    }

    #[test]
    fn test_instance_hash_format() {
        let h = instance_hash();
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
