use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ~/.ssh/config の1エントリを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshHost {
    /// Host エイリアス（例: "dev-server"）
    pub alias: String,
    /// 実際の接続先ホスト名 / IP
    pub hostname: String,
    pub port: u16,
    pub user: Option<String>,
    pub identity_file: Option<PathBuf>,
    /// ProxyJump（踏み台ホスト。例: "bastion" や "jump1,jump2"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    /// ControlMaster（"auto", "yes", "no" 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_master: Option<String>,
    /// ControlPath（ソケットパス。"none" は除外）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_path: Option<String>,
    /// ControlPersist（"yes", "no", 秒数）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_persist: Option<String>,
}

/// ~/.ssh/config をパースしてホスト一覧を返す（UIのリスト表示用）。
/// ワイルドカード（Host *）は除外する。
pub fn load() -> anyhow::Result<Vec<SshHost>> {
    let config_path = home_dir()?.join(".ssh/config");
    if !config_path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(&config_path)?;
    Ok(parse(&content))
}

/// `ssh -G alias` を実行し、出力テキストを返す。
pub fn run_ssh_g(alias: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("ssh")
        .args(["-G", alias])
        .output()
        .map_err(|e| anyhow::anyhow!("ssh コマンドの実行に失敗しました: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ssh -G {alias} が失敗しました: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// `ssh -G alias` を実行し、OpenSSH が解決した設定を取得する。
/// Include / Match / ProxyJump 等すべてのディレクティブが解決済みの状態で返る。
pub fn resolve(alias: &str) -> anyhow::Result<SshHost> {
    let stdout = run_ssh_g(alias)?;
    parse_ssh_g(alias, &stdout)
}

/// `ssh -G` の出力をパースして SshHost を構築する。
/// 出力形式: "key value\n" が行ごとに並ぶ（全て小文字キー）。
fn parse_ssh_g(alias: &str, output: &str) -> anyhow::Result<SshHost> {
    let mut hostname = alias.to_string();
    let mut port: u16 = 22;
    let mut user: Option<String> = None;
    let mut identity_file: Option<PathBuf> = None;
    let mut proxy_jump: Option<String> = None;
    let mut control_master: Option<String> = None;
    let mut control_path: Option<String> = None;
    let mut control_persist: Option<String> = None;

    for line in output.lines() {
        let (key, value) = match line.split_once(' ') {
            Some(kv) => kv,
            None => continue,
        };

        match key {
            "hostname" => hostname = value.to_string(),
            "port" => {
                if let Ok(p) = value.parse::<u16>() {
                    port = p;
                }
            }
            "user" => user = Some(value.to_string()),
            "identityfile" => {
                let path = expand_tilde(value);
                if path.exists() {
                    identity_file = Some(path);
                }
            }
            "proxyjump" => {
                if value != "none" && !value.is_empty() {
                    proxy_jump = Some(value.to_string());
                }
            }
            "controlmaster" => {
                control_master = Some(value.to_string());
            }
            "controlpath" => {
                if value != "none" && !value.is_empty() {
                    control_path = Some(value.to_string());
                }
            }
            "controlpersist" => {
                control_persist = Some(value.to_string());
            }
            _ => {}
        }
    }

    Ok(SshHost {
        alias: alias.to_string(),
        hostname,
        port,
        user,
        identity_file,
        proxy_jump,
        control_master,
        control_path,
        control_persist,
    })
}

/// ccc が ControlMaster を自分で立てるときに使う ControlPath テンプレート。
/// `%C` は ssh が hostname/port/user のハッシュに展開する。
pub const CCC_CONTROL_PATH: &str = "~/.ssh/ccc-cm-%C";

/// ユーザーの `~/.ssh/config` に当該 alias 向けの ControlMaster 設定が有効な形で
/// 入っているか判定する。`"no" / "none" / "false"` 以外を有効とみなす。
///
/// 有効な場合、ccc は専用 master を立てない（ユーザー側の master/socket を尊重する）。
pub fn user_has_control_master(alias: &str) -> anyhow::Result<bool> {
    let resolved = resolve(alias)?;
    Ok(resolved
        .control_master
        .as_deref()
        .is_some_and(|v| !matches!(v, "no" | "none" | "false")))
}

/// ccc 専用 ControlMaster を起動するための ssh 引数を組み立てる。
///
/// `-M -N -f` でバックグラウンドのマスター専用 ssh を立て、reverse forward
/// (`-R port:127.0.0.1:port`) を **このマスターにのみ** 付ける。`ExitOnForwardFailure=yes`
/// を入れているため、リモート側で port bind に失敗した場合は ssh ごと即座に終了し、
/// 沈黙故障にならない。
///
/// 各インスタンス用 ssh は `build_slave_ssh_args(_, true, ..)` で生成し、同じ
/// ControlPath を `-S` で参照することで forward を共有する。
pub fn build_master_ssh_args(alias: &str, hook_port: u16) -> anyhow::Result<Vec<String>> {
    // resolve は単に alias の妥当性確認のため呼ぶ（既存 build_ssh_args_with_forward と同じ作法）
    let _ = resolve(alias)?;
    Ok(vec![
        "-M".into(),
        "-N".into(),
        "-f".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        format!("ControlPath={CCC_CONTROL_PATH}"),
        "-o".into(),
        "ControlPersist=30".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-R".into(),
        format!("127.0.0.1:{hook_port}:127.0.0.1:{hook_port}"),
        alias.into(),
    ])
}

/// 各インスタンス用の ssh 引数を組み立てる。
///
/// - `ccc_master = true`: ccc 専用 master が事前に立っている前提。`-S` で
///   `CCC_CONTROL_PATH` を参照し、master に multiplex する。forward は master が
///   既に持っているのでここでは付けない（`hook_port` は無視）。
/// - `ccc_master = false`: ユーザー側 ControlMaster を尊重するケース。`-S` は
///   付けず、ユーザー設定の ControlPath/ControlMaster=auto が `ssh -G` 経由で
///   適用されるのに任せる。forward はユーザー側 master の最初の 1 本に
///   集約されることを期待し、`hook_port = Some(port)` のとき `-R` を付ける。
///   複数同時起動時にレースが残るが、ccc 側で集約する手段がない以上、最初に
///   通った 1 本で全インスタンスが救われる従来挙動を維持する（health check で
///   通らないケースは可視化される）。
pub fn build_slave_ssh_args(
    alias: &str,
    ccc_master: bool,
    hook_port: Option<u16>,
) -> anyhow::Result<Vec<String>> {
    let _ = resolve(alias)?;
    let mut args: Vec<String> = vec!["-t".into()];
    args.extend(["-o".into(), "StrictHostKeyChecking=accept-new".into()]);

    if ccc_master {
        args.extend(["-o".into(), format!("ControlPath={CCC_CONTROL_PATH}")]);
    } else if let Some(port) = hook_port {
        args.extend(["-R".into(), format!("127.0.0.1:{port}:127.0.0.1:{port}")]);
    }

    args.push(alias.into());
    Ok(args)
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME environment variable not set"))
}

fn parse(content: &str) -> Vec<SshHost> {
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut current: Option<SshHost> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (key, value) = match split_kv(trimmed) {
            Some(kv) => kv,
            None => continue,
        };

        match key.to_ascii_lowercase().as_str() {
            "host" => {
                if let Some(h) = current.take() {
                    hosts.push(h);
                }
                if value.contains('*') || value.contains('?') {
                    current = None;
                } else {
                    current = Some(SshHost {
                        alias: value.to_string(),
                        hostname: value.to_string(),
                        port: 22,
                        user: None,
                        identity_file: None,
                        proxy_jump: None,
                        control_master: None,
                        control_path: None,
                        control_persist: None,
                    });
                }
            }
            "hostname" => {
                if let Some(ref mut h) = current {
                    h.hostname = value.to_string();
                }
            }
            "port" => {
                if let Some(ref mut h) = current {
                    if let Ok(p) = value.parse::<u16>() {
                        h.port = p;
                    }
                }
            }
            "user" => {
                if let Some(ref mut h) = current {
                    h.user = Some(value.to_string());
                }
            }
            "identityfile" => {
                if let Some(ref mut h) = current {
                    let path = expand_tilde(value);
                    h.identity_file = Some(path);
                }
            }
            "proxyjump" => {
                if let Some(ref mut h) = current {
                    if value != "none" && !value.is_empty() {
                        h.proxy_jump = Some(value.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(h) = current {
        hosts.push(h);
    }

    hosts
}

fn split_kv(s: &str) -> Option<(&str, &str)> {
    if let Some(pos) = s.find('=') {
        Some((s[..pos].trim(), s[pos + 1..].trim()))
    } else if let Some(pos) = s.find(|c: char| c.is_ascii_whitespace()) {
        Some((s[..pos].trim(), s[pos..].trim()))
    } else {
        None
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let config = r#"
Host dev
    HostName 192.168.1.10
    Port 2222
    User ubuntu
    IdentityFile ~/.ssh/id_ed25519

Host prod
    HostName prod.example.com
    User deploy
    ProxyJump bastion

Host *
    ServerAliveInterval 60
"#;
        let hosts = parse(config);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "dev");
        assert_eq!(hosts[0].hostname, "192.168.1.10");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[0].user, Some("ubuntu".to_string()));
        assert_eq!(hosts[0].proxy_jump, None);
        assert_eq!(hosts[1].alias, "prod");
        assert_eq!(hosts[1].port, 22);
        assert_eq!(hosts[1].proxy_jump, Some("bastion".to_string()));
    }

    #[test]
    fn test_parse_ssh_g() {
        let output = "\
hostname 10.0.0.5
port 22
user ubuntu
identityfile ~/.ssh/id_ed25519
proxyjump bastion
";
        let host = parse_ssh_g("dev-host", output).unwrap();
        assert_eq!(host.alias, "dev-host");
        assert_eq!(host.hostname, "10.0.0.5");
        assert_eq!(host.port, 22);
        assert_eq!(host.user, Some("ubuntu".to_string()));
        assert_eq!(host.proxy_jump, Some("bastion".to_string()));
    }

    #[test]
    fn test_parse_ssh_g_no_proxy() {
        let output = "\
hostname direct.example.com
port 2222
user deploy
proxyjump none
";
        let host = parse_ssh_g("direct", output).unwrap();
        assert_eq!(host.hostname, "direct.example.com");
        assert_eq!(host.port, 2222);
        assert_eq!(host.proxy_jump, None);
    }

    #[test]
    fn test_parse_ssh_g_control_master() {
        let output = "\
hostname 10.0.0.5
port 22
user ubuntu
controlmaster auto
controlpath /tmp/ssh-%r@%h:%p
controlpersist 60
";
        let host = parse_ssh_g("dev", output).unwrap();
        assert_eq!(host.control_master, Some("auto".to_string()));
        assert_eq!(host.control_path, Some("/tmp/ssh-%r@%h:%p".to_string()));
        assert_eq!(host.control_persist, Some("60".to_string()));
    }

    #[test]
    fn test_parse_ssh_g_control_master_defaults() {
        let output = "\
hostname 10.0.0.5
port 22
controlmaster no
controlpath none
controlpersist no
";
        let host = parse_ssh_g("dev", output).unwrap();
        assert_eq!(host.control_master, Some("no".to_string()));
        assert_eq!(host.control_path, None); // "none" は除外
        assert_eq!(host.control_persist, Some("no".to_string()));
    }

    #[test]
    fn test_user_has_control_master_no() {
        // ssh -G を実行せず parse_ssh_g の結果を直接判定ロジックに通す
        let output = "\
hostname 10.0.0.5
port 22
controlmaster no
";
        let host = parse_ssh_g("dev", output).unwrap();
        let has_cm = host
            .control_master
            .as_deref()
            .is_some_and(|v| !matches!(v, "no" | "none" | "false"));
        assert!(
            !has_cm,
            "ControlMaster=no はアクティブでないと判定されるべき"
        );
    }

    #[test]
    fn test_user_has_control_master_auto() {
        let output = "\
hostname 10.0.0.5
port 22
controlmaster auto
controlpath /tmp/ssh-%C
controlpersist 30
";
        let host = parse_ssh_g("dev", output).unwrap();
        let has_cm = host
            .control_master
            .as_deref()
            .is_some_and(|v| !matches!(v, "no" | "none" | "false"));
        assert!(has_cm, "ControlMaster=auto はアクティブと判定されるべき");
    }

    // ── build_master_ssh_args / build_slave_ssh_args ────────────────────
    //
    // resolve() は ssh -G を実走するので、CI/サンドボックス環境を考慮して
    // ここでは「組み立て要素の検証」のみテストする（args 配列の組み立て自体は
    // 純粋関数で、resolve の戻り値に依存しない）。実 ssh が解決可能な
    // host alias を前提とした smoke test は手動 E2E に委ねる。

    #[test]
    fn control_path_template_uses_hash_token() {
        // `%C` は ssh の hostname/port/user/luser ハッシュ展開トークン。
        // 値が壊れていないことを保証する（破壊的変更時の検出）。
        assert!(CCC_CONTROL_PATH.contains("%C"));
        assert!(CCC_CONTROL_PATH.starts_with("~/.ssh/"));
    }
}
