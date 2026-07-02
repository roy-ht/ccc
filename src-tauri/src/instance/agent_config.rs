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
/// `.credentials.json`:
/// - rsync では `--exclude=.credentials.json` で常に除外する
/// - sidecar `ccc-claude-auth` 経由で macOS Keychain から取得し、
///   `~/.ccc/agent_settings/claude/<profile>/.credentials.json` として scp で送信
/// - Keychain にエントリがない場合は警告ログのみで起動を継続する
///   (リモート側で `claude /login` する想定)
///
/// 戻り値: リモート側の絶対パス文字列 (`~/.ccc/agent_settings/claude/<profile>`)。
pub fn prepare_remote_claude_config(
    host_alias: &str,
    profile: &str,
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

    // rsync 完了後にリモート側のプラグイン状態を CLI で再構築する。
    // 失敗しても Claude Code 起動は継続できるよう、エラーは警告ログのみ。
    let remote_profile_dir = format!("{remote_home}/.ccc/agent_settings/claude/{profile}");
    restore_remote_plugins(
        host_alias,
        &remote_profile_dir,
        &local_profile_dir,
        log_path,
    );

    // Keychain → 一時ファイル → scp で `.credentials.json` を送信
    match fetch_credentials_via_sidecar(&local_profile_dir) {
        Ok(creds) => {
            let tmp_path = std::env::temp_dir()
                .join(format!("ccc-cred-{}-{profile}.json", std::process::id()));
            if let Err(e) = std::fs::write(&tmp_path, &creds) {
                eprintln!("[ccc] credentials の一時書き出し失敗: {e}");
                debug_log::append(
                    log_path,
                    &format!("[prepare_config] credentials 一時書き出し失敗: {e}"),
                );
            } else {
                let t = std::time::Instant::now();
                let target =
                    format!("{host_alias}:.ccc/agent_settings/claude/{profile}/.credentials.json");
                debug_log::append(
                    log_path,
                    &format!("[prepare_config] scp .credentials.json 開始: → {target}"),
                );
                let scp_result = Command::new("scp")
                    .args(["-q", "-p"])
                    .arg(&tmp_path)
                    .arg(&target)
                    .status();
                let _ = std::fs::remove_file(&tmp_path);
                let elapsed = t.elapsed().as_millis();
                match scp_result {
                    Ok(s) if s.success() => {
                        debug_log::append(
                            log_path,
                            &format!("[prepare_config] scp .credentials.json 完了 (+{elapsed}ms)"),
                        );
                    }
                    Ok(s) => {
                        debug_log::append(
                            log_path,
                            &format!(
                                "[prepare_config] scp .credentials.json 失敗 (+{elapsed}ms, status={s})"
                            ),
                        );
                        eprintln!("[ccc] .credentials.json の scp が失敗: {s}");
                    }
                    Err(e) => {
                        debug_log::append(
                            log_path,
                            &format!(
                                "[prepare_config] scp .credentials.json エラー (+{elapsed}ms): {e}"
                            ),
                        );
                        eprintln!("[ccc] scp 起動失敗: {e}");
                    }
                }
            }
        }
        Err(e) => {
            debug_log::append(
                log_path,
                &format!("[prepare_config] Keychain credentials 取得失敗: {e}"),
            );
            eprintln!(
                "[ccc] Keychain から credentials を取得できませんでした (profile={profile}): {e} \
                 — リモートで `claude /login` を実行してください。"
            );
        }
    }

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

fn fetch_credentials_via_sidecar(config_dir: &Path) -> anyhow::Result<Vec<u8>> {
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
