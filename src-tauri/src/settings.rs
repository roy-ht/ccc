use serde::{Deserialize, Serialize};

use crate::paths;

// ─── データ構造 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub display: DisplaySettings,
    #[serde(default)]
    pub connections: Vec<ConnectionPreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplaySettings {
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: u16,
    #[serde(default = "default_color_theme")]
    pub color_theme: String,
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    /// サイドバーの状態メッセージ表示行数（高さ固定、はみ出しは clip）
    #[serde(default = "default_status_message_lines")]
    pub status_message_lines: u8,
    /// ターミナルの WebGL レンダラを使うか。WKWebView の WebGL 合成に不具合がある
    /// 環境（例: macOS 26.5 の描画回帰 = xterm.js#5816。テキスト選択位置のずれ・
    /// 描画乱れとして現れる）向けの逃げ道として false = DOM レンダラに切替できる。
    #[serde(default = "default_use_webgl")]
    pub use_webgl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionPreset {
    pub id: String,
    pub name: String,
    pub target: PresetTarget,
    #[serde(default = "default_command")]
    pub command: String,
    #[serde(default)]
    pub directory: String,
    /// 使用する agent_settings プロファイル名（`~/.ccc/agent_settings/claude/<name>/`）。
    /// 既存プリセット (フィールド未設定) は "default" として扱う。
    #[serde(default = "default_agent_profile")]
    pub agent_profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresetTarget {
    Local,
    Remote(RemoteConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// 接続用のホストエイリアス（~/.ssh/config の Host 名）
    pub host_alias: String,
}

// ─── デフォルト値 ────────────────────────────────────────────────────────────

fn default_font_family() -> String {
    r#""Cascadia Code", "Fira Code", "JetBrains Mono", monospace"#.to_string()
}

fn default_font_size() -> u16 {
    14
}

fn default_color_theme() -> String {
    "dark".to_string()
}

fn default_scrollback_lines() -> u32 {
    3000
}

fn default_status_message_lines() -> u8 {
    2
}

fn default_use_webgl() -> bool {
    true
}

fn default_command() -> String {
    "claude".to_string()
}

fn default_agent_profile() -> String {
    "default".to_string()
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_size: default_font_size(),
            color_theme: default_color_theme(),
            scrollback_lines: default_scrollback_lines(),
            status_message_lines: default_status_message_lines(),
            use_webgl: default_use_webgl(),
        }
    }
}

// ─── ファイル I/O ────────────────────────────────────────────────────────────

/// 設定を読み込む。ファイルが存在しない場合はデフォルト値を返す。
pub fn load() -> anyhow::Result<AppSettings> {
    let path = paths::settings_path()?;
    if !path.exists() {
        return Ok(AppSettings::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let settings: AppSettings = serde_json::from_str(&content)?;
    Ok(settings)
}

/// 設定を保存する。ディレクトリが存在しない場合は作成する。
pub fn save(settings: &AppSettings) -> anyhow::Result<()> {
    let path = paths::settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        assert_eq!(settings.display.font_size, 14);
        assert_eq!(settings.display.color_theme, "dark");
        assert_eq!(settings.display.scrollback_lines, 3000);
        assert!(settings.connections.is_empty());
    }

    #[test]
    fn test_serialize_deserialize() {
        let settings = AppSettings {
            display: DisplaySettings::default(),
            connections: vec![
                ConnectionPreset {
                    id: "test-1".to_string(),
                    name: "My Local".to_string(),
                    target: PresetTarget::Local,
                    command: "claude".to_string(),
                    directory: "~/projects".to_string(),
                    agent_profile: "default".to_string(),
                },
                ConnectionPreset {
                    id: "test-2".to_string(),
                    name: "Dev Host".to_string(),
                    target: PresetTarget::Remote(RemoteConfig {
                        host_alias: "dev-host".to_string(),
                    }),
                    command: "claude".to_string(),
                    directory: "/home/user/work".to_string(),
                    agent_profile: "work".to_string(),
                },
            ],
        };

        let json = serde_json::to_string_pretty(&settings).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.connections.len(), 2);
        assert_eq!(restored.connections[0].name, "My Local");
        assert_eq!(restored.connections[0].agent_profile, "default");
        assert_eq!(restored.connections[1].agent_profile, "work");
    }

    /// 古い settings.json (agent_profile フィールドなし) を読み込んでも
    /// "default" にフォールバックする (後方互換性)。
    #[test]
    fn test_legacy_preset_defaults_agent_profile() {
        let json = r#"{
            "display": {
                "font_family": "monospace",
                "font_size": 14,
                "color_theme": "dark",
                "scrollback_lines": 3000
            },
            "connections": [{
                "id": "legacy-1",
                "name": "Legacy",
                "target": { "type": "local" },
                "command": "claude",
                "directory": ""
            }]
        }"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.connections[0].agent_profile, "default");
        // use_webgl フィールドが無い旧設定は既定 true にフォールバックする
        assert!(settings.display.use_webgl);
    }
}
