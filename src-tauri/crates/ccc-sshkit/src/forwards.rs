//! SSH port forwarding 管理。
//!
//! SSH には forward を列挙する手段がないため（10.3 の `-O channels` も listener を
//! 出さないことを実機確認済み）、ccc 経由で追加した forward をホスト単位の台帳
//! `~/.ccc[/dev]/forwards/<alias>.json` に記録し、`ssh -G` の LocalForward と
//! hook 用予約 (-R) を合成して一覧を作る。
//!
//! master が作り直されると ccc 追加分は消えるため、台帳エントリに適用先 master の
//! pid を記録し、世代交代（pid 変化）を検知したら全件 `-O forward` でリプレイする。
//! 同一 forward の重複追加は master 側で冪等なので、リプレイは無条件で流してよい。

use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::exec::run_with_timeout;
use crate::ssh_config;

/// mux コマンド（-O check/forward/cancel/exit）の上限。ローカルの control socket
/// 越し操作なので健全なら数 ms〜（-O forward -R のみサーバ応答待ち）数百 ms。
/// master プロセスが固まっている（wedged）場合にここで打ち切る。
pub const MUX_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// 台帳ファイルの読み書きを直列化するプロセス内ロック。
/// 操作頻度が低い（UI 操作・60 秒サイクル）ため全ホスト共通の 1 本で足りる。
/// プロセス間（GUI と CLI の同時操作）の競合は「最後の書き込みが勝つ」を許容する。
static LEDGER_LOCK: Mutex<()> = Mutex::new(());

// ─── 型 ──────────────────────────────────────────────────────────────────────

/// `-L listen:dest` 1 本分の指定。listen_host 省略時は ssh の既定（localhost）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_host: Option<String>,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
}

impl ForwardSpec {
    /// `-L` に渡す `[bind:]port:host:hostport` 形式
    fn to_l_arg(&self) -> String {
        match &self.listen_host {
            Some(h) => format!(
                "{h}:{}:{}:{}",
                self.listen_port, self.dest_host, self.dest_port
            ),
            None => format!("{}:{}:{}", self.listen_port, self.dest_host, self.dest_port),
        }
    }

    /// listen 側の同一性（同じ listen に 2 本は張れないため、これを台帳のキーとする）
    fn listen_key(&self) -> (Option<&str>, u16) {
        (self.listen_host.as_deref(), self.listen_port)
    }
}

/// 台帳エントリ。`master_pid` は最後に適用が成功した master の pid。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub spec: ForwardSpec,
    /// 適用先 master の pid（世代）。現 master と不一致なら失効。
    pub master_pid: Option<u32>,
    /// 直近のリプレイ/適用エラー（None = 正常）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 追加時刻（unix 秒）
    pub created_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// 台帳の対象ホスト alias。ファイル名は `sanitize_alias` 済みで元に戻せないため、
    /// 横断一覧でホストを列挙できるよう中に保持する。旧形式（欠落）はファイル名で代替する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    entries: Vec<LedgerEntry>,
}

/// 一覧 1 行。フロントの `ForwardRow` と対応。
#[derive(Debug, Clone, Serialize)]
pub struct ForwardRow {
    pub spec: ForwardSpec,
    /// "ledger"（ccc 追加）/ "config"（ssh config の LocalForward）/ "reserved"（hook 用）
    pub origin: String,
    /// reverse forward (-R) なら true（hook 予約のみ）
    pub reverse: bool,
    /// 現 master に適用されていない（master 不在・世代不一致・リプレイ失敗）
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub deletable: bool,
}

// ─── 台帳の永続化 ─────────────────────────────────────────────────────────────

/// alias をファイル名に使えるよう、パス区切りだけ無害化する。
pub(crate) fn sanitize_alias(host_alias: &str) -> String {
    host_alias
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

fn ledger_path(host_alias: &str) -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::forwards_dir()?.join(format!("{}.json", sanitize_alias(host_alias))))
}

fn load_ledger(host_alias: &str) -> Ledger {
    let Ok(path) = ledger_path(host_alias) else {
        return Ledger::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ledger::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_ledger(host_alias: &str, ledger: &Ledger) -> Result<(), String> {
    let path = ledger_path(host_alias).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳ディレクトリ作成失敗: {e}"))?;
    }
    // alias は常に現在のホスト名で書き直す（旧形式のファイルもこれで補完される）。
    let to_write = Ledger {
        alias: Some(host_alias.to_string()),
        entries: ledger.entries.clone(),
    };
    let json = serde_json::to_string_pretty(&to_write).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("台帳書き込み失敗: {e}"))
}

/// 台帳ファイルを持つホスト alias を列挙する。
///
/// インスタンスが 1 つも起動していないホストの forward を見つけるための入口。
/// `alias` フィールドが無い旧形式の台帳はファイル名（sanitize 済み）で代替する。
pub fn ledger_hosts() -> Vec<String> {
    let Ok(dir) = crate::paths::forwards_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(ledger) = serde_json::from_str::<Ledger>(&content) else {
            continue;
        };
        // 空の台帳はホストとして数えない（削除後の残骸）
        if ledger.entries.is_empty() {
            continue;
        }
        let alias = ledger.alias.clone().or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        });
        if let Some(a) = alias {
            hosts.push(a);
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

// ─── mux コマンド ─────────────────────────────────────────────────────────────

/// mux コマンド用の ControlPath 引数。ユーザー CM 尊重モードでは ssh -G 経由の
/// ユーザー設定に任せ（引数なし）、ccc 専用 master のときだけ明示する。
/// `agent_socket`（gpg forward 修復）も同じ引数を使う。
pub fn mux_base_args(host_alias: &str) -> Result<Vec<String>, String> {
    let user_cm = ssh_config::user_has_control_master(host_alias)
        .map_err(|e| format!("ssh -G による設定解決に失敗: {e}"))?;
    if user_cm {
        Ok(vec![])
    } else {
        Ok(vec![
            "-o".into(),
            format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
        ])
    }
}

/// `ssh -O check` から現 master の pid を取得する。master 不在/不通/無応答は None。
/// 出力例: "Master running (pid=25742)"（stderr に出る）。
pub fn master_pid(host_alias: &str) -> Option<u32> {
    match master_pid_detailed(host_alias)? {
        MasterCheck::Running { pid } => pid,
        MasterCheck::NotRunning | MasterCheck::Wedged => None,
    }
}

/// `-O check` の詳細分類。liveness 層が wedged（socket 無応答）を区別するのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterCheck {
    /// master 応答あり（pid はパース失敗時 None）
    Running { pid: Option<u32> },
    /// master 不在（socket なし・接続拒否）
    NotRunning,
    /// control socket が応答しない（master プロセスが固まっている）
    Wedged,
}

/// `-O check` を実行して master の状態を分類する。ssh の起動自体に失敗したら None。
pub fn master_pid_detailed(host_alias: &str) -> Option<MasterCheck> {
    let base = mux_base_args(host_alias).ok()?;
    let output = run_with_timeout(
        Command::new("ssh")
            .args(&base)
            .args(["-O", "check", host_alias]),
        MUX_OP_TIMEOUT,
    )
    .ok()?;
    if output.timed_out {
        return Some(MasterCheck::Wedged);
    }
    if output.code != Some(0) {
        return Some(MasterCheck::NotRunning);
    }
    let text = format!("{}{}", output.stdout, output.stderr);
    Some(MasterCheck::Running {
        pid: parse_pid(&text),
    })
}

fn parse_pid(text: &str) -> Option<u32> {
    let start = text.find("pid=")? + 4;
    let digits: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// `-O forward` / `-O cancel` を実行する。失敗時は stderr をそのまま返す
/// （フロントでエラー表示する。例: ポートが他プロセスに使用されている）。
fn run_mux_forward(host_alias: &str, op: &str, spec: &ForwardSpec) -> Result<(), String> {
    let base = mux_base_args(host_alias)?;
    let output = run_with_timeout(
        Command::new("ssh")
            .args(&base)
            .args(["-O", op, "-L", &spec.to_l_arg(), host_alias]),
        MUX_OP_TIMEOUT,
    )
    .map_err(|e| format!("ssh の起動に失敗: {e}"))?;
    if output.success() {
        Ok(())
    } else if output.timed_out {
        Err(format!(
            "ssh -O {op} が {} 秒以内に応答しませんでした（master が half-open の可能性）",
            MUX_OP_TIMEOUT.as_secs()
        ))
    } else {
        let stderr = output.stderr.trim().to_string();
        let msg = if stderr.is_empty() {
            format!(
                "ssh -O {op} が非ゼロ終了: {}",
                output.code.map_or("signal".to_string(), |c| c.to_string())
            )
        } else if stderr.contains("Port forwarding failed") {
            // master 側の bind 失敗。実エラー (Address already in use 等) は
            // master プロセスの stderr に出るためここには届かない。
            format!(
                "listen ポート {} が既に使用中です（別プロセス、または別ホストの \
                 ssh master が同じポートを forward している可能性）: {stderr}",
                spec.listen_port
            )
        } else if stderr.contains("Control socket connect") {
            // master 不在 or `ssh -O stop` でソケットだけ消えた状態。
            format!(
                "control master に到達できません。master が終了したか、`ssh -O stop` で \
                 ソケットが削除されています。ホストへ接続し直すと新しい master が立ち、\
                 ccc 追加分の forward は自動で再適用されます: {stderr}"
            )
        } else {
            stderr
        };
        Err(msg)
    }
}

// ─── 操作（台帳と mux の同期） ─────────────────────────────────────────────────

/// forward を追加して台帳に記録する。同一 listen のエントリは上書き（再試行を兼ねる）。
pub fn add(host_alias: &str, spec: ForwardSpec) -> Result<(), String> {
    let _guard = LEDGER_LOCK.lock().unwrap();
    run_mux_forward(host_alias, "forward", &spec)?;
    let pid = master_pid(host_alias);
    let mut ledger = load_ledger(host_alias);
    ledger
        .entries
        .retain(|e| e.spec.listen_key() != spec.listen_key());
    ledger.entries.push(LedgerEntry {
        spec,
        master_pid: pid,
        error: None,
        created_at: now_epoch(),
    });
    save_ledger(host_alias, &ledger)
}

/// ccc 台帳にある forward を削除する。台帳にない spec は拒否する
/// （config 定義分・hook 予約・外部管理のものを ccc から消させない）。
pub fn remove(host_alias: &str, spec: ForwardSpec) -> Result<(), String> {
    let _guard = LEDGER_LOCK.lock().unwrap();
    let mut ledger = load_ledger(host_alias);
    let Some(pos) = ledger
        .entries
        .iter()
        .position(|e| e.spec.listen_key() == spec.listen_key())
    else {
        return Err("ccc が管理していない forward は削除できません".into());
    };
    let entry = ledger.entries[pos].clone();

    if let Err(e) = run_mux_forward(host_alias, "cancel", &entry.spec) {
        // master が世代交代していて実体が無い（=失効済み）なら台帳から消すだけでよい
        let stale = master_pid(host_alias) != entry.master_pid;
        if !stale {
            return Err(e);
        }
    }
    ledger.entries.remove(pos);
    save_ledger(host_alias, &ledger)
}

/// master の世代交代を検知したら台帳をリプレイする（冪等なので無条件で流す）。
///
/// - master 不在: 何もしない（一覧側で失効表示になる）
/// - pid 不一致のエントリのみ `-O forward` を再発行し、成功なら pid 更新、
///   失敗なら error を記録（pid は古いまま = 失効表示が続く）
pub fn sync_ledger(host_alias: &str) {
    let _guard = LEDGER_LOCK.lock().unwrap();
    let Some(pid) = master_pid(host_alias) else {
        return;
    };
    let mut ledger = load_ledger(host_alias);
    let mut changed = false;
    for entry in ledger.entries.iter_mut() {
        if entry.master_pid == Some(pid) && entry.error.is_none() {
            continue;
        }
        match run_mux_forward(host_alias, "forward", &entry.spec) {
            Ok(()) => {
                entry.master_pid = Some(pid);
                entry.error = None;
            }
            Err(e) => {
                entry.error = Some(e);
            }
        }
        changed = true;
    }
    if changed {
        let _ = save_ledger(host_alias, &ledger);
    }
}

/// 一覧を組み立てる（台帳 + ssh -G の LocalForward + hook 予約）。
/// 呼び出し側（コマンド層）が直前に `sync_ledger` を呼ぶ想定。
pub fn list(host_alias: &str, hook_port: Option<u16>) -> Result<Vec<ForwardRow>, String> {
    let _guard = LEDGER_LOCK.lock().unwrap();
    let current_pid = master_pid(host_alias);
    let ledger = load_ledger(host_alias);

    let mut rows: Vec<ForwardRow> = ledger
        .entries
        .iter()
        .map(|e| {
            let stale = current_pid.is_none() || e.master_pid != current_pid || e.error.is_some();
            // 失効エントリの listen ポートがまだ塞がっている場合は「ゾンビ」
            // （`ssh -O stop` 後の旧 master や他プロセスが保持）として区別する。
            // bind を一瞬試すだけなので外部ツール不要（誤検知は許容）。
            let error = e.error.clone().or_else(|| {
                (stale && !local_port_is_free(e.spec.listen_port)).then(|| {
                    "listen ポートが解放されていません（`ssh -O stop` された旧 master \
                     または他プロセスが保持中の可能性）"
                        .to_string()
                })
            });
            ForwardRow {
                spec: e.spec.clone(),
                origin: "ledger".into(),
                reverse: false,
                stale,
                error,
                deletable: true,
            }
        })
        .collect();

    // ssh config の LocalForward（master 起動時に自動で開く。master 不在時のみ失効扱い）
    let config_specs = config_local_forwards(host_alias)?;
    for spec in config_specs {
        if rows
            .iter()
            .any(|r| r.spec.listen_key() == spec.listen_key())
        {
            continue; // 台帳と同じ listen は台帳側の表示を優先
        }
        rows.push(ForwardRow {
            spec,
            origin: "config".into(),
            reverse: false,
            stale: current_pid.is_none(),
            error: None,
            deletable: false,
        });
    }

    // hook 用 reverse forward（ccc 予約。空きポート把握のために表示する）
    if let Some(port) = hook_port {
        rows.push(ForwardRow {
            spec: ForwardSpec {
                listen_host: Some("127.0.0.1".into()),
                listen_port: port,
                dest_host: "127.0.0.1".into(),
                dest_port: port,
            },
            origin: "reserved".into(),
            reverse: true,
            stale: current_pid.is_none(),
            error: None,
            deletable: false,
        });
    }

    rows.sort_by_key(|r| r.spec.listen_port);
    Ok(rows)
}

// ─── ホスト横断の一覧 ─────────────────────────────────────────────────────────

/// forward 1 本の実効状態。ホスト単位の `stale` に「listen ポートが実際に
/// 塞がっているか」を掛け合わせたもの。
///
/// 横断ビューでは「効いていない」だけでは足りず、**誰かに取られているのか
/// 単に空いているのか**が分からないとポート衝突を解決できない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardState {
    /// 現 master に適用されている（実際に転送が効いている）
    Active,
    /// 未適用で、listen ポートは別の何かが専有している
    Blocked,
    /// 未適用で、listen ポートは空いている（再適用すれば通る）
    Inactive,
}

/// 横断一覧の 1 行。`ForwardRow` にホストと衝突情報を足したもの。
#[derive(Debug, Clone, Serialize)]
pub struct GlobalForwardRow {
    pub host_alias: String,
    pub spec: ForwardSpec,
    /// "ledger" / "config" / "reserved"
    pub origin: String,
    pub reverse: bool,
    pub state: ForwardState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub deletable: bool,
    /// 同じ listen ポートを別のホストも登録している
    pub conflict: bool,
}

/// `stale`（master 世代不一致）と listen ポートの空き状況から実効状態を決める。
fn classify_state(stale: bool, port_free: bool) -> ForwardState {
    if !stale {
        ForwardState::Active
    } else if port_free {
        ForwardState::Inactive
    } else {
        ForwardState::Blocked
    }
}

/// 同じ listen ポートを 2 つ以上のホストが登録している行に `conflict` を立てる。
///
/// bind アドレス（`listen_host`）は判定に含めない。`0.0.0.0:8080` と
/// `127.0.0.1:8080` は実際には衝突するうえ、`None` / `localhost` / `127.0.0.1`
/// が同じものを指すため、ポート番号だけで判定した方が実態に合う。
///
/// 同一ホスト内の重複は台帳のキー制約（`listen_key`）で起きないため、
/// ホストを跨ぐ場合のみ衝突とみなす。`reserved`（ccc の hook 用予約）は
/// ホストに紐づかないので判定から除く。
fn mark_conflicts(rows: &mut [GlobalForwardRow]) {
    use std::collections::{HashMap, HashSet};
    let mut hosts_by_port: HashMap<u16, HashSet<&str>> = HashMap::new();
    for r in rows.iter().filter(|r| r.origin != "reserved") {
        hosts_by_port
            .entry(r.spec.listen_port)
            .or_default()
            .insert(r.host_alias.as_str());
    }
    let conflicting: HashSet<u16> = hosts_by_port
        .into_iter()
        .filter(|(_, hosts)| hosts.len() > 1)
        .map(|(port, _)| port)
        .collect();
    for r in rows.iter_mut() {
        r.conflict = r.origin != "reserved" && conflicting.contains(&r.spec.listen_port);
    }
}

/// 複数ホストの forward を 1 つの一覧に合成する。
///
/// 各ホストについて `sync_ledger`（世代チェック＋リプレイ）を行ってから
/// `list` を呼ぶ。hook 用の reverse forward はホストに依存しないので、
/// ホストごとに重複させず最後に 1 行だけ足す。
///
/// 並び順は listen ポート昇順 → ホスト名。衝突している行が隣り合うので
/// UI 側でグルーピングしやすい。
pub fn list_all(hosts: &[String], hook_port: Option<u16>) -> Vec<GlobalForwardRow> {
    let mut rows: Vec<GlobalForwardRow> = Vec::new();

    for host in hosts {
        sync_ledger(host);
        // hook 予約はここでは足さない（下でまとめて 1 行）
        let Ok(host_rows) = list(host, None) else {
            continue;
        };
        for r in host_rows {
            let port_free = local_port_is_free(r.spec.listen_port);
            rows.push(GlobalForwardRow {
                host_alias: host.clone(),
                state: classify_state(r.stale, port_free),
                spec: r.spec,
                origin: r.origin,
                reverse: r.reverse,
                error: r.error,
                deletable: r.deletable,
                conflict: false,
            });
        }
    }

    if let Some(port) = hook_port {
        rows.push(GlobalForwardRow {
            host_alias: String::new(),
            spec: ForwardSpec {
                listen_host: Some("127.0.0.1".into()),
                listen_port: port,
                dest_host: "127.0.0.1".into(),
                dest_port: port,
            },
            origin: "reserved".into(),
            reverse: true,
            state: ForwardState::Active,
            error: None,
            deletable: false,
            conflict: false,
        });
    }

    mark_conflicts(&mut rows);
    rows.sort_by(|a, b| {
        a.spec
            .listen_port
            .cmp(&b.spec.listen_port)
            .then_with(|| a.host_alias.cmp(&b.host_alias))
    });
    rows
}

/// `ssh -G` の `localforward` 行をパースする。
/// 出力例: `localforward 3000 [localhost]:3000` / `localforward [127.0.0.1]:8080 [db]:5432`
fn config_local_forwards(host_alias: &str) -> Result<Vec<ForwardSpec>, String> {
    let stdout = ssh_config::run_ssh_g(host_alias).map_err(|e| e.to_string())?;
    Ok(stdout
        .lines()
        .filter_map(|line| line.strip_prefix("localforward "))
        .filter_map(parse_localforward)
        .collect())
}

fn parse_localforward(value: &str) -> Option<ForwardSpec> {
    let (listen, dest) = value.trim().split_once(' ')?;
    let (listen_host, listen_port) = parse_fwd_part(listen)?;
    let (dest_host, dest_port) = parse_fwd_part(dest)?;
    Some(ForwardSpec {
        listen_host,
        listen_port,
        dest_host: dest_host.unwrap_or_else(|| "localhost".into()),
        dest_port,
    })
}

/// `[host]:port` / `host:port` / `port` を (host, port) に分解する。
fn parse_fwd_part(part: &str) -> Option<(Option<String>, u16)> {
    let part = part.trim();
    if let Some(rest) = part.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((Some(host.to_string()), port.parse().ok()?));
    }
    if let Some((host, port)) = part.rsplit_once(':') {
        return Some((Some(host.to_string()), port.parse().ok()?));
    }
    Some((None, part.parse().ok()?))
}

/// listen ポートが現在ローカルで空いているか（bind を一瞬試して判定）。
fn local_port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_localforward_plain_port() {
        let spec = parse_localforward("3000 [localhost]:3000").unwrap();
        assert_eq!(spec.listen_host, None);
        assert_eq!(spec.listen_port, 3000);
        assert_eq!(spec.dest_host, "localhost");
        assert_eq!(spec.dest_port, 3000);
    }

    #[test]
    fn parse_localforward_with_bind_addr() {
        let spec = parse_localforward("[127.0.0.1]:8080 [db.internal]:5432").unwrap();
        assert_eq!(spec.listen_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(spec.listen_port, 8080);
        assert_eq!(spec.dest_host, "db.internal");
        assert_eq!(spec.dest_port, 5432);
    }

    #[test]
    fn l_arg_roundtrip() {
        let spec = ForwardSpec {
            listen_host: None,
            listen_port: 18080,
            dest_host: "localhost".into(),
            dest_port: 80,
        };
        assert_eq!(spec.to_l_arg(), "18080:localhost:80");
        let spec = ForwardSpec {
            listen_host: Some("0.0.0.0".into()),
            ..spec
        };
        assert_eq!(spec.to_l_arg(), "0.0.0.0:18080:localhost:80");
    }

    #[test]
    fn parse_pid_from_check_output() {
        assert_eq!(parse_pid("Master running (pid=25742)\n"), Some(25742));
        assert_eq!(parse_pid("Control socket connect failed"), None);
    }

    #[test]
    fn local_port_is_free_detects_bound_port() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!local_port_is_free(port));
        drop(listener);
        // macOS では socket() と FD_CLOEXEC 設定が atomic でないため、並列実行中の
        // 他テスト（exec の子プロセス spawn）の fork に listener fd が一時継承され、
        // drop 直後はまだポートが塞がっていることがある。子の exec/終了で解放される
        // ので短時間リトライする（プロダクション側は元々誤検知許容の best-effort）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if local_port_is_free(port) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drop 後にポートが解放されない"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn sanitize_alias_replaces_path_separators() {
        assert_eq!(sanitize_alias("host/with\\sep"), "host_with_sep");
        assert_eq!(sanitize_alias("container-dev-host"), "container-dev-host");
    }

    // ─── 横断一覧 ────────────────────────────────────────────────────────────

    fn row(host: &str, port: u16, origin: &str) -> GlobalForwardRow {
        GlobalForwardRow {
            host_alias: host.into(),
            spec: ForwardSpec {
                listen_host: None,
                listen_port: port,
                dest_host: "localhost".into(),
                dest_port: port,
            },
            origin: origin.into(),
            reverse: origin == "reserved",
            state: ForwardState::Active,
            error: None,
            deletable: origin == "ledger",
            conflict: false,
        }
    }

    #[test]
    fn classify_state_covers_three_cases() {
        assert_eq!(classify_state(false, false), ForwardState::Active);
        assert_eq!(classify_state(false, true), ForwardState::Active);
        assert_eq!(classify_state(true, true), ForwardState::Inactive);
        assert_eq!(classify_state(true, false), ForwardState::Blocked);
    }

    #[test]
    fn conflicts_are_detected_across_hosts() {
        // 実際に観測されたケース: 同じ listen ポートを 2 ホストが登録している
        let mut rows = vec![
            row("host-a", 8766, "ledger"),
            row("host-b", 8766, "ledger"),
            row("host-a", 8080, "ledger"),
        ];
        mark_conflicts(&mut rows);
        assert!(rows[0].conflict);
        assert!(rows[1].conflict);
        assert!(!rows[2].conflict, "単独登録が衝突扱いになっている");
    }

    #[test]
    fn same_host_same_port_is_not_a_conflict() {
        // 台帳側は listen キーで一意なので、config 由来と重なっても同一ホスト内。
        // ホスト内の重複は衝突として煽らない。
        let mut rows = vec![row("host-a", 8080, "ledger"), row("host-a", 8080, "config")];
        mark_conflicts(&mut rows);
        assert!(!rows[0].conflict);
        assert!(!rows[1].conflict);
    }

    #[test]
    fn bind_address_does_not_split_conflict_detection() {
        // 0.0.0.0:8080 と 127.0.0.1:8080 は実際には衝突する
        let mut rows = vec![row("host-a", 8080, "ledger"), row("host-b", 8080, "ledger")];
        rows[0].spec.listen_host = Some("0.0.0.0".into());
        rows[1].spec.listen_host = Some("127.0.0.1".into());
        mark_conflicts(&mut rows);
        assert!(rows[0].conflict);
        assert!(rows[1].conflict);
    }

    #[test]
    fn reserved_row_is_excluded_from_conflicts() {
        // hook 用予約はホストに紐づかないので、同じポートがあっても衝突にしない
        let mut rows = vec![row("", 51397, "reserved"), row("host-a", 51397, "ledger")];
        mark_conflicts(&mut rows);
        assert!(!rows[0].conflict);
        assert!(
            !rows[1].conflict,
            "reserved を除くと host-a は単独なので衝突ではない"
        );
    }

    #[test]
    fn ledger_roundtrips_alias() {
        let json = r#"{"alias":"my-host","entries":[]}"#;
        let l: Ledger = serde_json::from_str(json).unwrap();
        assert_eq!(l.alias.as_deref(), Some("my-host"));
        // 旧形式（alias 無し）も読める
        let l: Ledger = serde_json::from_str(r#"{"entries":[]}"#).unwrap();
        assert_eq!(l.alias, None);
    }
}
