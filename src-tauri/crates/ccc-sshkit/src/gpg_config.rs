//! gpg relay の設定（`~/.ccc[/dev]/gpg.json`）。
//!
//! **gpg forward の定義は `~/.ssh/config` に書かない**（specs/v0.14 §11.1）。
//! config に定義が無ければ素の `ssh` はそもそも forward を要求できないため、
//! 「発動しないようにする」ではなく「発動しようがない」構造になる。
//!
//! 全ホスト分を 1 ファイルに持つ:
//!
//! ```json
//! {
//!   "schema": 1,
//!   "defaults": { "grace_secs": 5 },
//!   "hosts": {
//!     "mybox": { "enabled": true },
//!     "container-host": { "enabled": true, "remote_socket": "/run/user/1000/gnupg/S.gpg-agent" }
//!   }
//! }
//! ```
//!
//! キーの alias は **`ccc-ssh <host>` に渡すホスト指定**（`user@host` なら `@` の
//! 右側）で、forward 台帳や uplink の状態ファイルと同じ識別子。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 設定ファイルの schema 版。
pub const SCHEMA: u32 = 1;

/// grace の組み込み既定（specs/v0.14 §8）。
pub const DEFAULT_GRACE_SECS: u64 = 5;

/// ホスト単位の設定。未指定（`None`）は `defaults` → 組み込み既定の順に解決する。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// リモート側 socket。`None` なら relay が `$GNUPGHOME/S.gpg-agent` を自分で決める
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_socket: Option<String>,
    /// ローカル側の接続先。`None` なら `$GNUPGHOME/S.gpg-agent.extra`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpgConfig {
    pub schema: u32,
    #[serde(default)]
    pub defaults: HostSettings,
    /// キー順を安定させるため BTreeMap（差分が読みやすい）
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSettings>,
}

impl Default for GpgConfig {
    fn default() -> Self {
        GpgConfig {
            schema: SCHEMA,
            defaults: HostSettings::default(),
            hosts: BTreeMap::new(),
        }
    }
}

/// 解決済みのホスト設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    pub alias: String,
    /// リモート側 socket（`None` = relay の既定に任せる）
    pub remote_socket: Option<String>,
    /// uplink が接続するローカル socket（必ず解決する）
    pub local_socket: PathBuf,
    pub grace_secs: u64,
}

pub fn config_path() -> Result<PathBuf> {
    Ok(crate::paths::data_root()?.join("gpg.json"))
}

/// 設定を読む。ファイルが無ければ空の設定を返す。
pub fn load() -> Result<GpgConfig> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<GpgConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} の解析に失敗しました", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GpgConfig::default()),
        Err(e) => Err(e).with_context(|| format!("{} を読めません", path.display())),
    }
}

/// 設定を保存する（atomic write: tmp + rename）。
pub fn save(config: &GpgConfig) -> Result<()> {
    save_to(&config_path()?, config)
}

pub fn save_to(path: &Path, config: &GpgConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("{} の更新に失敗しました", path.display()))?;
    Ok(())
}

impl GpgConfig {
    /// 当該 alias で relay が有効か。
    pub fn is_enabled(&self, alias: &str) -> bool {
        self.hosts
            .get(alias)
            .and_then(|h| h.enabled)
            .or(self.defaults.enabled)
            .unwrap_or(false)
    }

    /// 有効なホストの設定を解決する。無効なら `None`。
    pub fn resolve(&self, alias: &str) -> Result<Option<ResolvedHost>> {
        if !self.is_enabled(alias) {
            return Ok(None);
        }
        let host = self.hosts.get(alias);
        let pick_str = |f: fn(&HostSettings) -> &Option<String>| -> Option<String> {
            host.and_then(|h| f(h).clone())
                .or_else(|| f(&self.defaults).clone())
        };
        let grace = host
            .and_then(|h| h.grace_secs)
            .or(self.defaults.grace_secs)
            .unwrap_or(DEFAULT_GRACE_SECS);
        let local_socket = match pick_str(|h| &h.local_socket) {
            Some(p) => PathBuf::from(expand_tilde(&p)?),
            None => default_local_socket()?,
        };
        Ok(Some(ResolvedHost {
            alias: alias.to_string(),
            remote_socket: pick_str(|h| &h.remote_socket),
            local_socket,
            grace_secs: grace,
        }))
    }

    /// 有効なホストの alias 一覧。
    pub fn enabled_hosts(&self) -> Vec<String> {
        self.hosts
            .keys()
            .filter(|alias| self.is_enabled(alias))
            .cloned()
            .collect()
    }

    /// 有効・無効を切り替える（エントリが無ければ作る）。
    pub fn set_enabled(&mut self, alias: &str, enabled: bool) {
        self.hosts.entry(alias.to_string()).or_default().enabled = Some(enabled);
    }
}

/// ローカル側の既定 socket。restricted socket（`.extra`）を使う。
///
/// 非 restricted の `S.gpg-agent` を転送すると、リモートから鍵の削除や
/// パスフレーズ変更まで届いてしまう。従来の運用（v0.9 以来）と同じ。
pub fn default_local_socket() -> Result<PathBuf> {
    Ok(gnupg_home()?.join("S.gpg-agent.extra"))
}

pub fn gnupg_home() -> Result<PathBuf> {
    match std::env::var("GNUPGHOME") {
        Ok(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => {
            let home =
                std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME が設定されていません"))?;
            Ok(PathBuf::from(home).join(".gnupg"))
        }
    }
}

/// 先頭の `~/` だけを展開する（設定ファイルに書かれたパス向け）。
fn expand_tilde(path: &str) -> Result<String> {
    let Some(rest) = path.strip_prefix("~/") else {
        return Ok(path.to_string());
    };
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME が設定されていません"))?;
    Ok(format!("{home}/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(hosts: &[(&str, HostSettings)], defaults: HostSettings) -> GpgConfig {
        GpgConfig {
            schema: SCHEMA,
            defaults,
            hosts: hosts
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    fn enabled() -> HostSettings {
        HostSettings {
            enabled: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn unknown_host_is_disabled() {
        let cfg = GpgConfig::default();
        assert!(!cfg.is_enabled("nope"));
        assert_eq!(cfg.resolve("nope").unwrap(), None);
    }

    #[test]
    fn host_entry_enables_relay() {
        let cfg = cfg_with(&[("mybox", enabled())], HostSettings::default());
        assert!(cfg.is_enabled("mybox"));
        let resolved = cfg.resolve("mybox").unwrap().unwrap();
        assert_eq!(resolved.alias, "mybox");
        assert_eq!(resolved.grace_secs, DEFAULT_GRACE_SECS);
        assert_eq!(resolved.remote_socket, None, "既定は relay に任せる");
    }

    #[test]
    fn host_setting_overrides_defaults() {
        let cfg = cfg_with(
            &[(
                "mybox",
                HostSettings {
                    enabled: Some(true),
                    grace_secs: Some(30),
                    ..Default::default()
                },
            )],
            HostSettings {
                grace_secs: Some(7),
                ..Default::default()
            },
        );
        assert_eq!(cfg.resolve("mybox").unwrap().unwrap().grace_secs, 30);
    }

    #[test]
    fn defaults_apply_when_host_omits_the_key() {
        let cfg = cfg_with(
            &[("mybox", enabled())],
            HostSettings {
                grace_secs: Some(7),
                ..Default::default()
            },
        );
        assert_eq!(cfg.resolve("mybox").unwrap().unwrap().grace_secs, 7);
    }

    #[test]
    fn host_can_disable_even_when_defaults_enable() {
        let cfg = cfg_with(
            &[(
                "mybox",
                HostSettings {
                    enabled: Some(false),
                    ..Default::default()
                },
            )],
            enabled(),
        );
        assert!(
            !cfg.is_enabled("mybox"),
            "ホスト設定が defaults より優先される"
        );
    }

    #[test]
    fn set_enabled_creates_and_toggles_entries() {
        let mut cfg = GpgConfig::default();
        cfg.set_enabled("mybox", true);
        assert!(cfg.is_enabled("mybox"));
        cfg.set_enabled("mybox", false);
        assert!(!cfg.is_enabled("mybox"));
        assert!(cfg.hosts.contains_key("mybox"), "エントリ自体は残る");
    }

    #[test]
    fn enabled_hosts_lists_only_active_ones() {
        let mut cfg = GpgConfig::default();
        cfg.set_enabled("a", true);
        cfg.set_enabled("b", false);
        cfg.set_enabled("c", true);
        assert_eq!(cfg.enabled_hosts(), vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("ccc-gpgcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("gpg.json");

        // 無ければ空の設定
        assert_eq!(load_from(&path).unwrap(), GpgConfig::default());

        let mut cfg = GpgConfig::default();
        cfg.set_enabled("mybox", true);
        save_to(&path, &cfg).unwrap();
        assert_eq!(load_from(&path).unwrap(), cfg);

        // 一時ファイルを残さない
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimal_json_is_accepted() {
        // 実質 {"enabled": true} だけで足りること
        let text = r#"{"schema":1,"hosts":{"mybox":{"enabled":true}}}"#;
        let cfg: GpgConfig = serde_json::from_str(text).unwrap();
        assert!(cfg.is_enabled("mybox"));
    }

    #[test]
    fn expand_tilde_only_touches_leading_home() {
        std::env::set_var("HOME", "/home/user");
        assert_eq!(expand_tilde("~/.gnupg/S").unwrap(), "/home/user/.gnupg/S");
        assert_eq!(expand_tilde("/abs/~/x").unwrap(), "/abs/~/x");
        assert_eq!(expand_tilde("relative").unwrap(), "relative");
    }
}
