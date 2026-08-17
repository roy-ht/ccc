use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hook_setup;
use crate::paths;

/// ローカルで `CLAUDE_CONFIG_DIR` として使うパスを返す。
///
/// `~/.ccc/agent_settings/claude/<profile>/` を直接 `CLAUDE_CONFIG_DIR` として
/// 注入する。コピーは行わない。Claude Code 自身が書き込む `.claude.json` や
/// `.credentials.json` もこのディレクトリに格納されるため、同じプロファイルを使う
/// 全インスタンスで認証・状態が自然に共有される。
///
/// 副作用: ディレクトリ作成と同時に、その配下の `settings.json` に ccc の
/// hook 定義を冪等マージする。merge に失敗しても Claude 起動自体は止めず、
/// 警告ログのみ残す。
pub fn local_claude_config_dir(profile: &str) -> anyhow::Result<PathBuf> {
    let dir = paths::claude_agent_settings_dir(profile)?;
    std::fs::create_dir_all(&dir)?;
    match hook_setup::settings_json::merge_into_config_dir(&dir) {
        Ok(true) => eprintln!(
            "[ccc] {} に hook 定義を追加しました",
            dir.join("settings.json").display()
        ),
        Ok(false) => {}
        Err(e) => eprintln!(
            "[ccc] {} の hook merge に失敗: {e}",
            dir.join("settings.json").display()
        ),
    }
    Ok(dir)
}

/// ローカル `~/.ccc/agent_settings/` 全体を rsync でリモート
/// `~/.ccc/agent_settings/` に同期し、Keychain の `.credentials.json` を
/// 別途 scp で送信する。
///
/// rsync を必須とする方針。ローカル単独 manifest 方式は不整合リスクが
/// あるため採用しない。rsync が見つからない場合は明示的にインストール手順を
/// 案内するエラーで止まる。
///
/// Symlink の扱い:
/// - 解決先が `~/.ccc/agent_settings/` 配下を指す相対 symlink → そのまま転送
///   (rsync `--safe-links`)
/// - 絶対 symlink / 外部参照 / broken → 警告して除外
///
/// 資格情報ファイル:
/// - rsync では `--exclude=.credentials.json` で常に除外する
/// - sidecar `ccc-claude-auth` 経由で macOS Keychain から取得し、
///   リモートの値と比較した上で**必要なときだけ** scp で送信する
///   (判定ロジックは [`super::auth_sync`]、転送は [`sync_remote_auth`])
/// - Keychain にエントリがない場合は警告ログのみで起動を継続する
///   (リモート側で `claude /login` する想定)
///
/// `live_sibling` は「同じホスト × 同じプロファイルで既に生きている ccc インスタンス
/// があるか」。リモートの状態を確認できなかったときの判定を安全側に倒すために使う。
///
/// 戻り値: リモート側の絶対パス文字列 (`~/.ccc/agent_settings/claude/<profile>`)。
pub fn prepare_remote_claude_config(
    host_alias: &str,
    profile: &str,
    live_sibling: bool,
    log_path: Option<&Path>,
) -> anyhow::Result<String> {
    use super::debug_log;
    use std::time::Instant;

    let total_start = Instant::now();
    debug_log::append(
        log_path,
        &format!("[prepare_config] 開始: host={host_alias}, profile={profile}"),
    );

    let agent_settings_root = paths::agent_settings_dir()?;
    let local_profile_dir = local_claude_config_dir(profile)?;

    // リモートに hook バイナリ (`~/.ccc/bin/ccc-claude-code-hook`) を配信する。
    // settings.json の `command` がチルダ表記でリモート側でホーム展開されるため、
    // バイナリ自体もリモートのホームに置く必要がある。
    let hook_install_start = Instant::now();
    match hook_setup::binary::install_remote(host_alias) {
        Ok(true) => debug_log::append(
            log_path,
            &format!(
                "[prepare_config] リモート hook バイナリ配信完了 (+{}ms)",
                hook_install_start.elapsed().as_millis()
            ),
        ),
        Ok(false) => debug_log::append(
            log_path,
            "[prepare_config] リモート hook バイナリは最新のためスキップ",
        ),
        Err(e) => {
            eprintln!("[ccc] リモート '{host_alias}' に hook バイナリ配信失敗: {e}");
            debug_log::append(
                log_path,
                &format!("[prepare_config] リモート hook バイナリ配信失敗: {e}"),
            );
        }
    }

    // 危険な symlink (絶対パス / 外部参照 / broken) を事前にスキャンして警告する。
    // 実際の除外は rsync `--safe-links` に任せる。
    let scan_start = Instant::now();
    let issues = scan_unsafe_symlinks(&agent_settings_root);
    for (path, msg) in &issues {
        let line = format!("{}: {msg}", path.display());
        eprintln!("[ccc] {line}");
        debug_log::append(log_path, &format!("[prepare_config] {line}"));
    }
    debug_log::append(
        log_path,
        &format!(
            "[prepare_config] symlink スキャン完了 (+{}ms, 警告 {} 件)",
            scan_start.elapsed().as_millis(),
            issues.len()
        ),
    );

    // リモート側の親ディレクトリを先に確保しつつ、リモートの `$HOME` を取得する。
    // `CLAUDE_CONFIG_DIR` を tmux の `-e` で注入する際、`~/` や `$HOME/` は
    // シェル展開されないまま claude プロセスに渡るため、絶対パスを使う必要がある。
    // `--rsync-path` トリックは非対話シェルの PATH 問題で exit 127 になりやすいため、
    // mkdir は独立した ssh 呼び出しで実行する。
    let mkdir_start = Instant::now();
    debug_log::append(
        log_path,
        &format!(
            "[prepare_config] ssh mkdir 開始: {host_alias} mkdir -p ~/.ccc/agent_settings && echo $HOME"
        ),
    );
    let mkdir_out = Command::new("ssh")
        .args([
            host_alias,
            "mkdir -p ~/.ccc/agent_settings && printf %s \"$HOME\"",
        ])
        .output()?;
    debug_log::append(
        log_path,
        &format!(
            "[prepare_config] ssh mkdir 完了 (+{}ms, status={})",
            mkdir_start.elapsed().as_millis(),
            mkdir_out.status
        ),
    );
    if !mkdir_out.status.success() {
        anyhow::bail!("ssh mkdir failed for {host_alias}");
    }
    let remote_home = String::from_utf8_lossy(&mkdir_out.stdout)
        .trim()
        .to_string();
    if remote_home.is_empty() || !remote_home.starts_with('/') {
        anyhow::bail!(
            "リモート '{host_alias}' の $HOME を取得できませんでした (got: {remote_home:?})"
        );
    }
    debug_log::append(
        log_path,
        &format!("[prepare_config] remote $HOME = {remote_home}"),
    );

    // rsync で agent_settings/ 全体をリモートに同期 (差分転送)。
    let rsync_start = Instant::now();
    let source = format!("{}/", agent_settings_root.display());
    let dest = format!("{host_alias}:.ccc/agent_settings/");
    debug_log::append(
        log_path,
        &format!("[prepare_config] rsync 開始: {source} → {dest}"),
    );
    // 転送量・ファイル数の削減:
    // - `-z` で圧縮 (jsonl/json/markdown 中心なので効果が大きい)
    // - リモート Claude の動作に不要なローカル状態 (会話ログ・キャッシュ・
    //   履歴・スナップショット類) を除外
    // - `plugins/` は丸ごと除外し、後段でリモート側 `claude plugin marketplace
    //   add` / `claude plugin install` で再構築する。理由: marketplaces 配下は
    //   git checkout の塊で帯域を強く支配する一方、絶対パスを含む json を
    //   そのまま運んでも整合せず、CLI 経由でリモートから直接 GitHub を引いた
    //   方が高速 (リモート→GitHub は遅トンネル経由しない)
    let rsync_result = Command::new("rsync")
        .args([
            "-az",
            "--safe-links",
            "--exclude=.credentials.json",
            // `.claude.json` は projects (絶対パスをキーとする trust 承諾 /
            // 許可済みツール / 入力履歴) を含むため丸ごと上書きすると
            // リモートの状態を壊す。sync_remote_claude_json で選択的にマージする。
            "--exclude=.claude.json",
            "--exclude=plugins/",
            "--exclude=projects/",
            "--exclude=cache/",
            "--exclude=shell-snapshots/",
            "--exclude=backups/",
            "--exclude=file-history/",
            "--exclude=sessions/",
            "--exclude=history.jsonl",
            "--exclude=stats-cache.json",
            "--exclude=mcp-needs-auth-cache.json",
        ])
        .arg(&source)
        .arg(&dest)
        .status();
    let rsync_elapsed = rsync_start.elapsed().as_millis();

    match rsync_result {
        Ok(s) if s.success() => {
            debug_log::append(
                log_path,
                &format!("[prepare_config] rsync 完了 (+{rsync_elapsed}ms)"),
            );
        }
        Ok(s) => {
            debug_log::append(
                log_path,
                &format!("[prepare_config] rsync 失敗 (+{rsync_elapsed}ms, status={s})"),
            );
            if s.code() == Some(127) {
                let msg = format!(
                    "リモートホスト '{host_alias}' に rsync がインストールされていません。\
                     リモートで以下のいずれかを実行してインストールしてください:\n\
                     \u{3000}- Debian/Ubuntu: sudo apt install rsync\n\
                     \u{3000}- RHEL/CentOS/Rocky: sudo yum install rsync\n\
                     \u{3000}- Alpine: sudo apk add rsync\n\
                     \u{3000}- Arch: sudo pacman -S rsync\n\
                     既にインストール済みの場合は、非対話 ssh の PATH に rsync が含まれているか \
                     `ssh {host_alias} 'command -v rsync'` で確認してください。"
                );
                eprintln!("[ccc] {msg}");
                anyhow::bail!(msg);
            }
            anyhow::bail!("rsync failed for {host_alias}: {s}");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug_log::append(
                log_path,
                &format!("[prepare_config] ローカルに rsync が見つかりません: {e}"),
            );
            let msg = "ローカルマシンに rsync がインストールされていません。\
                       macOS: `brew install rsync`、Debian/Ubuntu: `sudo apt install rsync` \
                       などでインストールしてください。";
            eprintln!("[ccc] {msg}");
            anyhow::bail!(msg);
        }
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[prepare_config] rsync エラー (+{rsync_elapsed}ms): {e}"),
            );
            return Err(anyhow::anyhow!("rsync 実行エラー: {e}"));
        }
    }

    let remote_profile_dir = format!("{remote_home}/.ccc/agent_settings/claude/{profile}");

    // `.claude.json` の選択的マージ。プラグイン復元より先に行う:
    // - `claude plugin ...` は `.claude.json` を書き換えるため、後にマージすると
    //   その結果を古い読み取り結果で巻き戻してしまう
    // - `hasCompletedOnboarding` を先に置くことで、非対話の `claude plugin`
    //   実行がオンボーディングで止まるのを防げる
    sync_remote_claude_json(
        host_alias,
        profile,
        &remote_profile_dir,
        &local_profile_dir,
        live_sibling,
        log_path,
    );

    // リモート側のプラグイン状態を CLI で再構築する。
    // 失敗しても Claude Code 起動は継続できるよう、エラーは警告ログのみ。
    restore_remote_plugins(
        host_alias,
        &remote_profile_dir,
        &local_profile_dir,
        log_path,
    );

    sync_remote_auth(
        host_alias,
        profile,
        &remote_profile_dir,
        &local_profile_dir,
        live_sibling,
        log_path,
    );

    debug_log::append(
        log_path,
        &format!(
            "[prepare_config] 完了 (合計+{}ms)",
            total_start.elapsed().as_millis()
        ),
    );
    // tmux の `-e CLAUDE_CONFIG_DIR=...` でそのまま渡されるため、絶対パスを返す。
    Ok(remote_profile_dir)
}

/// 資格情報ファイルをリモートへ同期する。
///
/// ローカル (Keychain) とリモートのメタ情報を比較し、**リモートを古い資格情報で
/// 上書きしない**ことを保証した上で、必要なときだけ転送する。判定は
/// [`super::auth_sync::decide`] を参照。
///
/// 転送は「一時ファイルへ scp → リモートで `chmod 600` → `mv -f`」の 3 段で行う。
/// `.credentials.json` へ直接 scp すると、リモートで走行中の Claude が
/// 切り詰められた JSON を読む窓が生まれるため。`mv` は同一ファイルシステム上の
/// rename なので原子的。
///
/// 失敗は全て警告ログのみ。Claude Code の起動自体は止めない
/// (最悪リモートで `claude /login` すれば復旧できる)。
fn sync_remote_auth(
    host_alias: &str,
    profile: &str,
    remote_profile_dir: &str,
    local_profile_dir: &Path,
    live_sibling: bool,
    log_path: Option<&Path>,
) {
    use super::auth_sync;
    use super::debug_log;

    let local_bytes = match fetch_local_auth_via_sidecar(local_profile_dir) {
        Ok(b) => b,
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[auth_sync] Keychain からの取得に失敗: {e}"),
            );
            eprintln!(
                "[ccc] Keychain から資格情報を取得できませんでした (profile={profile}): {e} \
                 — リモートで `claude /login` を実行してください。"
            );
            return;
        }
    };

    let local_meta = match auth_sync::parse(&local_bytes) {
        Ok(m) => m,
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[auth_sync] ローカル資格情報を解析できないため転送しない: {e}"),
            );
            eprintln!("[ccc] ローカル資格情報を解析できませんでした: {e}");
            return;
        }
    };

    let remote_state = match read_remote_file(
        host_alias,
        &format!("{remote_profile_dir}/.credentials.json"),
        "auth_sync",
        log_path,
    ) {
        RemoteFile::Missing => auth_sync::RemoteState::Missing,
        RemoteFile::Unknown => auth_sync::RemoteState::Unknown,
        RemoteFile::Present(bytes) => match auth_sync::parse(&bytes) {
            Ok(meta) => auth_sync::RemoteState::Present(meta),
            Err(e) => {
                debug_log::append(
                    log_path,
                    &format!("[auth_sync] リモート資格情報を解析できない: {e}"),
                );
                auth_sync::RemoteState::Unknown
            }
        },
    };
    debug_log::append(
        log_path,
        &format!(
            "[auth_sync] local(expires_at={}, refresh_expires_at={:?}, fp={}) / remote={} / live_sibling={live_sibling}",
            local_meta.expires_at,
            local_meta.refresh_token_expires_at,
            local_meta.refresh_fp,
            describe_remote_state(&remote_state),
        ),
    );

    let verdict = auth_sync::decide(
        &local_meta,
        &remote_state,
        live_sibling,
        auth_sync::now_ms(),
    );
    debug_log::append(
        log_path,
        &format!(
            "[auth_sync] 判定: {} — {}",
            if verdict.send {
                "転送する"
            } else {
                "転送しない"
            },
            verdict.reason
        ),
    );
    if verdict.warn_user {
        eprintln!("[ccc] {}", verdict.reason);
    }
    if !verdict.send {
        return;
    }

    if let Err(e) = push_remote_file_atomically(
        host_alias,
        profile,
        remote_profile_dir,
        ".credentials.json",
        &local_bytes,
        "auth_sync",
        log_path,
    ) {
        debug_log::append(log_path, &format!("[auth_sync] 転送に失敗: {e}"));
        eprintln!("[ccc] 資格情報の転送に失敗: {e}");
    }
}

/// `.claude.json` をリモートへ選択的にマージする。
///
/// rsync では `--exclude=.claude.json` で除外し、代わりにこの関数が
/// [`super::claude_json::SYNCED_KEYS`] のトップレベルキーだけをローカル値で
/// 上書きする。リモートの `projects`（絶対パスをキーとする trust 承諾 /
/// 許可済みツール / 入力履歴）は完全に保持される。
///
/// 走行中の Claude はこのファイルを読み書きするため、`live_sibling` のときは
/// 一切触らない。必要なキーは前回接続時に入っている。
///
/// 失敗は全て警告ログのみで、Claude Code の起動は止めない。
fn sync_remote_claude_json(
    host_alias: &str,
    profile: &str,
    remote_profile_dir: &str,
    local_profile_dir: &Path,
    live_sibling: bool,
    log_path: Option<&Path>,
) {
    use super::claude_json;
    use super::debug_log;

    if live_sibling {
        debug_log::append(
            log_path,
            "[claude_json] 同一ホスト・プロファイルで稼働中のインスタンスがあるためスキップ",
        );
        return;
    }

    let local_path = local_profile_dir.join(".claude.json");
    let local_raw = match std::fs::read(&local_path) {
        Ok(b) => b,
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[claude_json] ローカルに .claude.json が無いためスキップ: {e}"),
            );
            return;
        }
    };
    let local_value: serde_json::Value = match serde_json::from_slice(&local_raw) {
        Ok(v) => v,
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[claude_json] ローカル .claude.json を解析できないためスキップ: {e}"),
            );
            return;
        }
    };

    // リモート側の現在値。読めない場合は状態を壊しうるので触らない。
    let remote_value = match read_remote_file(
        host_alias,
        &format!("{remote_profile_dir}/.claude.json"),
        "claude_json",
        log_path,
    ) {
        RemoteFile::Missing => serde_json::Value::Null,
        RemoteFile::Present(bytes) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                debug_log::append(
                    log_path,
                    &format!("[claude_json] リモート .claude.json を解析できないためスキップ: {e}"),
                );
                return;
            }
        },
        RemoteFile::Unknown => {
            debug_log::append(
                log_path,
                "[claude_json] リモートの状態を確認できないためスキップ",
            );
            return;
        }
    };

    let Some(result) = claude_json::merge(&local_value, &remote_value) else {
        debug_log::append(log_path, "[claude_json] 差分なし — 書き込みをスキップ");
        return;
    };
    debug_log::append(
        log_path,
        &format!("[claude_json] 更新するキー: {:?}", result.updated_keys),
    );

    let bytes = match serde_json::to_vec_pretty(&result.merged) {
        Ok(b) => b,
        Err(e) => {
            debug_log::append(log_path, &format!("[claude_json] JSON 生成に失敗: {e}"));
            return;
        }
    };
    if let Err(e) = push_remote_file_atomically(
        host_alias,
        profile,
        remote_profile_dir,
        ".claude.json",
        &bytes,
        "claude_json",
        log_path,
    ) {
        debug_log::append(log_path, &format!("[claude_json] 転送に失敗: {e}"));
        eprintln!("[ccc] .claude.json の同期に失敗: {e}");
    }
}

/// リモートファイルの読み取り結果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteFile {
    /// ファイルが存在しない。
    Missing,
    /// 読み取れた（中身は秘密を含みうるのでログに出さない）。
    Present(Vec<u8>),
    /// ssh 失敗・権限エラーなど、状態を判定できなかった。
    Unknown,
}

/// リモートのファイルを読み、3 状態に分類する。
///
/// 終了コードで区別する:
/// - 0 + stdout あり → `Present`
/// - 3 → ファイルが存在しない (`Missing`)
/// - その他 (ssh 失敗、権限エラー、0 バイトファイル等) → `Unknown`
///
/// stdout には資格情報や入力履歴が載りうるため、**内容は一切ログに出さない**。
fn read_remote_file(
    host_alias: &str,
    remote_path: &str,
    tag: &str,
    log_path: Option<&Path>,
) -> RemoteFile {
    use super::debug_log;

    let script = format!(
        "if [ -f {p} ]; then cat {p}; else exit 3; fi",
        p = sh_quote(remote_path)
    );
    let t = std::time::Instant::now();
    let out = match Command::new("ssh")
        .args(["-T", host_alias])
        .arg(&script)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            debug_log::append(log_path, &format!("[{tag}] ssh 起動失敗: {e}"));
            return RemoteFile::Unknown;
        }
    };
    let elapsed = t.elapsed().as_millis();

    match out.status.code() {
        Some(0) if !out.stdout.is_empty() => {
            debug_log::append(
                log_path,
                &format!(
                    "[{tag}] リモートファイルを取得 (+{elapsed}ms, {} bytes)",
                    out.stdout.len()
                ),
            );
            RemoteFile::Present(out.stdout)
        }
        Some(3) => {
            debug_log::append(
                log_path,
                &format!("[{tag}] リモートにファイルなし (+{elapsed}ms)"),
            );
            RemoteFile::Missing
        }
        code => {
            debug_log::append(
                log_path,
                &format!("[{tag}] リモートファイルの確認に失敗 (+{elapsed}ms, status={code:?})"),
            );
            RemoteFile::Unknown
        }
    }
}

/// 一時ファイル経由で原子的にリモートへ配置する。
///
/// ローカル一時ファイルは 0600 で作成し、成功・失敗いずれの経路でも削除する。
/// リモートでも `chmod 600` してから `mv -f` する。`mv` は同一ファイルシステム上の
/// rename なので原子的で、走行中の Claude が中途半端なファイルを読むことがない。
fn push_remote_file_atomically(
    host_alias: &str,
    profile: &str,
    remote_profile_dir: &str,
    file_name: &str,
    bytes: &[u8],
    tag: &str,
    log_path: Option<&Path>,
) -> anyhow::Result<()> {
    use super::debug_log;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let pid = std::process::id();
    let local_tmp = std::env::temp_dir().join(format!("ccc-{tag}-{pid}-{profile}.json"));
    // 直前のクラッシュ等で残っていた場合、`mode()` は既存ファイルに適用されないので
    // 必ず消してから作り直す。
    let _ = std::fs::remove_file(&local_tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&local_tmp)
            .map_err(|e| anyhow::anyhow!("一時ファイルの作成に失敗: {e}"))?;
        f.write_all(bytes)
            .map_err(|e| anyhow::anyhow!("一時ファイルの書き出しに失敗: {e}"))?;
    }

    // scp のリモートパスは既存実装と同じくホーム相対で指定する。
    // OpenSSH 9 以降は SFTP モードが既定でリモートシェル展開が効かないため、
    // クォートを付けずホーム相対のまま渡すのが最も互換性が高い。
    let remote_tmp_rel = format!(".ccc/agent_settings/claude/{profile}/{file_name}.ccc-tmp");
    let target = format!("{host_alias}:{remote_tmp_rel}");
    let t = std::time::Instant::now();
    let scp_status = Command::new("scp")
        .args(["-q", "-p"])
        .arg(&local_tmp)
        .arg(&target)
        .status();
    let _ = std::fs::remove_file(&local_tmp);
    let elapsed = t.elapsed().as_millis();

    match scp_status {
        Ok(s) if s.success() => debug_log::append(
            log_path,
            &format!("[{tag}] scp 完了 (+{elapsed}ms) → {remote_tmp_rel}"),
        ),
        Ok(s) => anyhow::bail!("scp が失敗 (+{elapsed}ms, status={s})"),
        Err(e) => anyhow::bail!("scp の起動に失敗 (+{elapsed}ms): {e}"),
    }

    // chmod → mv を 1 回の ssh でまとめて実行する。
    let remote_tmp_abs = format!("{remote_profile_dir}/{file_name}.ccc-tmp");
    let remote_final = format!("{remote_profile_dir}/{file_name}");
    let script = format!(
        "chmod 600 {tmp} && mv -f {tmp} {dst}",
        tmp = sh_quote(&remote_tmp_abs),
        dst = sh_quote(&remote_final)
    );
    let t = std::time::Instant::now();
    let out = Command::new("ssh")
        .args(["-T", host_alias])
        .arg(&script)
        .output()
        .map_err(|e| anyhow::anyhow!("ssh の起動に失敗: {e}"))?;
    let elapsed = t.elapsed().as_millis();
    if !out.status.success() {
        // 中途半端な一時ファイルを残さない
        let cleanup = format!("rm -f {}", sh_quote(&remote_tmp_abs));
        let _ = Command::new("ssh")
            .args(["-T", host_alias])
            .arg(&cleanup)
            .status();
        anyhow::bail!(
            "リモートでの配置に失敗 (+{elapsed}ms, status={}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    debug_log::append(
        log_path,
        &format!("[{tag}] リモートへ原子的に配置完了 (+{elapsed}ms) → {remote_final}"),
    );
    Ok(())
}

/// ログ用にリモート状態を秘密なしで 1 行表現する。
fn describe_remote_state(state: &super::auth_sync::RemoteState) -> String {
    use super::auth_sync::RemoteState;
    match state {
        RemoteState::Missing => "missing".to_string(),
        RemoteState::Unknown => "unknown".to_string(),
        RemoteState::Present(m) => format!(
            "present(expires_at={}, refresh_expires_at={:?}, fp={})",
            m.expires_at, m.refresh_token_expires_at, m.refresh_fp
        ),
    }
}

/// POSIX sh の single quote で安全に囲む。`'` は `'\''` へ展開する。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// ローカルのプラグイン設定 (`plugins/known_marketplaces.json`,
/// `plugins/installed_plugins.json`) を読み、リモート側で
/// `claude plugin marketplace add` / `claude plugin install` を実行して
/// プラグイン状態を再構築する。
///
/// rsync 側で `plugins/` を丸ごと除外しているため、その分の帯域を稼ぎつつ
/// リモートから直接 GitHub に取りに行かせるのが目的。
///
/// 方針:
/// - 公式マーケットプレイス (`claude-plugins-official`) も CLI 経由では
///   明示的な `marketplace add` が必要なので登録する
/// - `installed_plugins.json` のうち `scope == "user"` のものだけ復元する
///   (project / local スコープはリポジトリの `.claude/settings.json` に
///   紐付くため、ここで復元しても意味がない)
/// - 失敗は警告ログのみ。Claude Code 起動は止めない (冪等再実行や
///   リモート側で `/plugin` から手動修復が可能)
fn restore_remote_plugins(
    host_alias: &str,
    remote_profile_dir: &str,
    local_profile_dir: &Path,
    log_path: Option<&Path>,
) {
    use super::debug_log;
    use std::time::Instant;

    let plugins_dir = local_profile_dir.join("plugins");
    let known_path = plugins_dir.join("known_marketplaces.json");
    let installed_path = plugins_dir.join("installed_plugins.json");

    if !known_path.exists() && !installed_path.exists() {
        debug_log::append(
            log_path,
            "[prepare_config] plugins 設定なし — 復元をスキップ",
        );
        return;
    }

    let total_start = Instant::now();
    debug_log::append(log_path, "[prepare_config] プラグイン復元 開始");

    // 1) marketplace を登録
    let known_value = std::fs::read_to_string(&known_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let marketplace_names: Vec<String> = known_value
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    if let Some(obj) = known_value.as_ref().and_then(|v| v.as_object()) {
        for (name, entry) in obj {
            let Some(source_arg) = format_marketplace_source(entry.get("source")) else {
                debug_log::append(
                    log_path,
                    &format!(
                        "[prepare_config] marketplace '{name}' は source 形式が \
                         サポート対象外のためスキップ"
                    ),
                );
                continue;
            };
            let t = Instant::now();
            // `command claude` でユーザーの alias/function をバイパスし、
            // PATH 解決された素の `claude` バイナリを直接呼び出す
            let cmd = format!(
                "CLAUDE_CONFIG_DIR={remote_profile_dir} \
                 command claude plugin marketplace add {source_arg}",
            );
            let result = run_remote_login_shell(host_alias, &cmd);
            let elapsed = t.elapsed().as_millis();
            match result {
                Ok((s, tail)) if s.success() => debug_log::append(
                    log_path,
                    &format!(
                        "[prepare_config] marketplace add '{name}' 完了 \
                         (+{elapsed}ms) out={tail}"
                    ),
                ),
                Ok((s, tail)) => debug_log::append(
                    log_path,
                    &format!(
                        "[prepare_config] marketplace add '{name}' 失敗 \
                         (+{elapsed}ms, status={s}) out={tail}"
                    ),
                ),
                Err(e) => debug_log::append(
                    log_path,
                    &format!(
                        "[prepare_config] marketplace add '{name}' エラー \
                         (+{elapsed}ms): {e}"
                    ),
                ),
            }
        }
    }

    // 2) installed_plugins.json から user scope のものを install
    if let Ok(s) = std::fs::read_to_string(&installed_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            let plugins = v
                .get("plugins")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            for (key, entries) in plugins {
                // key は "<name>@<marketplace>" 形式
                let Some(arr) = entries.as_array() else {
                    continue;
                };
                let has_user_scope = arr
                    .iter()
                    .any(|e| e.get("scope").and_then(|s| s.as_str()) == Some("user"));
                if !has_user_scope {
                    continue;
                }
                // marketplace 部分が known になければ install しても失敗する
                let Some(market) = key.split_once('@').map(|(_, m)| m) else {
                    continue;
                };
                if !marketplace_names.iter().any(|n| n == market) {
                    debug_log::append(
                        log_path,
                        &format!(
                            "[prepare_config] plugin install '{key}' スキップ \
                             (marketplace 未登録)"
                        ),
                    );
                    continue;
                }
                let t = Instant::now();
                let cmd = format!(
                    "CLAUDE_CONFIG_DIR={remote_profile_dir} \
                     command claude plugin install {key} --scope user",
                );
                let result = run_remote_login_shell(host_alias, &cmd);
                let elapsed = t.elapsed().as_millis();
                match result {
                    Ok((s, tail)) if s.success() => debug_log::append(
                        log_path,
                        &format!(
                            "[prepare_config] plugin install '{key}' 完了 \
                             (+{elapsed}ms) out={tail}"
                        ),
                    ),
                    Ok((s, tail)) => debug_log::append(
                        log_path,
                        &format!(
                            "[prepare_config] plugin install '{key}' 失敗 \
                             (+{elapsed}ms, status={s}) out={tail}"
                        ),
                    ),
                    Err(e) => debug_log::append(
                        log_path,
                        &format!(
                            "[prepare_config] plugin install '{key}' エラー \
                             (+{elapsed}ms): {e}"
                        ),
                    ),
                }
            }
        }
    }

    debug_log::append(
        log_path,
        &format!(
            "[prepare_config] プラグイン復元 完了 (+{}ms)",
            total_start.elapsed().as_millis()
        ),
    );
}

/// `known_marketplaces.json` の `source` フィールドを `claude plugin
/// marketplace add` の引数文字列に変換する。
///
/// 対応:
/// - `{"source": "github", "repo": "owner/name"}` → `owner/name`
/// - `{"source": "git", "url": "..."}` / `{"source": "url", "url": "..."}`
///   → `<url>` (必要なら `#ref` 付き)
/// - `local` などその他は None (リモートで再現できないためスキップ)
fn format_marketplace_source(source: Option<&serde_json::Value>) -> Option<String> {
    let s = source?.as_object()?;
    match s.get("source").and_then(|v| v.as_str())? {
        "github" => s.get("repo").and_then(|v| v.as_str()).map(String::from),
        "git" | "url" => s.get("url").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// 非対話 ssh で login shell 経由のコマンドを実行し、stdout/stderr の
/// 末尾を返す (デバッグログ向け)。
///
/// `claude` が `~/.local/bin` などにある場合に PATH を確実に通すために
/// `bash -l` を経由する。
///
/// 注意: ssh は複数の引数をスペースで連結してリモートに 1 つのコマンド
/// 文字列として送るため、`bash -l -c '<cmd>'` を **1 つの文字列** に
/// まとめてから ssh に渡す必要がある。分割して渡すと `-c` の引数が
/// 最初の単語のみになり、残りは位置パラメータ扱いされて実行されない。
fn run_remote_login_shell(
    host_alias: &str,
    command: &str,
) -> std::io::Result<(std::process::ExitStatus, String)> {
    // single quote 内に single quote を含めるための定石: ' → '\''
    let escaped = command.replace('\'', r"'\''");
    let remote_cmd = format!("bash -l -c '{escaped}'");
    let out = Command::new("ssh")
        .args(["-T", host_alias])
        .arg(&remote_cmd)
        .output()?;
    let mut tail = String::from_utf8_lossy(&out.stdout).to_string();
    tail.push_str(&String::from_utf8_lossy(&out.stderr));
    // ノイズになる行は除く
    let cleaned: String = tail
        .lines()
        .filter(|l| {
            !l.contains("Pseudo-terminal")
                && !l.starts_with("reset:")
                && !l.contains("Warning: Permanently added")
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let trimmed = if cleaned.len() > 400 {
        format!("{}…", &cleaned[..400])
    } else {
        cleaned
    };
    Ok((out.status, trimmed))
}

/// `root` 配下を再帰走査し、転送に問題のある symbolic link を
/// (パス, 警告メッセージ) として返す。
///
/// 検出対象:
/// - 絶対パスを指す symlink (cross-host で機能しない)
/// - 解決後の絶対パスが `root` 配下を外れる相対 symlink
/// - 解決できない (broken) symlink
///
/// rsync `--safe-links` の挙動と概ね一致する。
fn scan_unsafe_symlinks(root: &Path) -> Vec<(PathBuf, String)> {
    let mut issues = Vec::new();
    let canonical_root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => return issues,
    };
    walk(root, &mut |path| {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        if !metadata.file_type().is_symlink() {
            return;
        }
        let Ok(target) = std::fs::read_link(path) else {
            return;
        };
        if target.is_absolute() {
            issues.push((
                path.to_path_buf(),
                format!(
                    "絶対パスを指す symbolic link → {} (転送対象から除外)",
                    target.display()
                ),
            ));
            return;
        }
        let resolved = path.parent().unwrap_or(Path::new(".")).join(&target);
        match resolved.canonicalize() {
            Ok(canonical) => {
                if !canonical.starts_with(&canonical_root) {
                    issues.push((
                        path.to_path_buf(),
                        format!(
                            "agent_settings 外を指す symbolic link → {} (転送対象から除外)",
                            canonical.display()
                        ),
                    ));
                }
            }
            Err(_) => {
                issues.push((
                    path.to_path_buf(),
                    format!(
                        "壊れた symbolic link → {} (転送対象から除外)",
                        target.display()
                    ),
                ));
            }
        }
    });
    issues
}

/// `path` 配下を再帰走査して各エントリで `callback` を呼ぶ。
/// symlink 経由のディレクトリには降りない (ループ防止)。
fn walk(path: &Path, callback: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        callback(&entry_path);
        let Ok(metadata) = std::fs::symlink_metadata(&entry_path) else {
            continue;
        };
        let ft = metadata.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            walk(&entry_path, callback);
        }
    }
}

/// sidecar `ccc-claude-auth` 経由で macOS Keychain から資格情報を取り出す。
///
/// 戻り値はファイルへそのまま書ける生バイト列。呼び出し側は内容をログに出さないこと。
fn fetch_local_auth_via_sidecar(config_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let bin = paths::ccc_claude_auth_bin()?;
    let output = Command::new(&bin)
        .arg("get-credentials")
        .arg("--config-dir")
        .arg(config_dir)
        .output()
        .map_err(|e| anyhow::anyhow!("{}: {e}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ccc-claude-auth failed: {}", stderr.trim());
    }
    let mut bytes = output.stdout;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn local_claude_config_dir_creates_profile_directory() {
        let unique = format!("ccc-test-{}", uuid::Uuid::new_v4());
        let path = paths::claude_agent_settings_dir(&unique).unwrap();
        assert!(!path.exists(), "テスト前提: 一意プロファイル名で未作成");
        let created = local_claude_config_dir(&unique).unwrap();
        assert!(created.is_dir());
        assert_eq!(created, path);
        let _ = fs::remove_dir_all(&created);
    }

    #[test]
    fn scan_unsafe_symlinks_detects_absolute_outside_and_broken() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("ccc-test-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("agent_settings");
        let outside_dir = tmp.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("external.md"), "external").unwrap();
        fs::write(root.join("inside.md"), "inside").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/nested.md"), "nested").unwrap();

        // 安全: 内部完結する相対 symlink
        symlink("inside.md", root.join("safe-link")).unwrap();
        symlink("../inside.md", root.join("sub/safe-link")).unwrap();
        // 危険: 絶対 symlink (root 内を指していても cross-host 不可)
        symlink(root.join("inside.md"), root.join("absolute-link")).unwrap();
        // 危険: root を抜ける相対 symlink
        symlink("../outside/external.md", root.join("escape-link")).unwrap();
        // 危険: broken
        symlink(tmp.join("nonexistent"), root.join("broken-link")).unwrap();

        let issues = scan_unsafe_symlinks(&root);
        let names: Vec<String> = issues
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.contains(&"absolute-link".to_string()));
        assert!(names.contains(&"escape-link".to_string()));
        assert!(names.contains(&"broken-link".to_string()));
        // safe-link (root 直下と sub/ 配下) は警告対象に含まれない
        assert!(!names.contains(&"safe-link".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }
}
