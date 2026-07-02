//! Explorer タブが扱う共通型。フロント (`src/types.ts`) の Explorer 関連型と対応する。

use serde::Serialize;

/// ディレクトリ内の 1 エントリ。
#[derive(Debug, Clone, Serialize)]
pub struct FileNode {
    pub name: String,
    /// ルートからの POSIX 相対パス（例: "src/main.rs"）。空文字はルート自身。
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMeta {
    pub size: u64,
    pub mime: Option<String>,
    pub is_binary: bool,
}

/// `read_preview` のオプション。
#[derive(Debug, Clone)]
pub struct PreviewOpts {
    pub text_limit: u64,
    pub image_limit: u64,
    pub pdf_limit: u64,
}

impl Default for PreviewOpts {
    fn default() -> Self {
        Self {
            text_limit: 5 * 1024 * 1024,
            image_limit: 8 * 1024 * 1024,
            pdf_limit: 25 * 1024 * 1024,
        }
    }
}

/// プレビュー結果。種別ごとに tagged enum で表現する。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Preview {
    Text {
        content: String,
        language: Option<String>,
        truncated: bool,
        size: u64,
    },
    Markdown {
        content: String,
        truncated: bool,
        size: u64,
    },
    Image {
        mime: String,
        base64: String,
        size: u64,
    },
    Pdf {
        base64: String,
        size: u64,
    },
    Binary {
        size: u64,
        mime: Option<String>,
    },
    TooLarge {
        size: u64,
        limit: u64,
    },
}

#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub max_results: usize,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self { max_results: 500 }
    }
}

/// 1 つの行マッチ。
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub line_number: u64,
    pub line: String,
    pub match_start: usize,
    pub match_end: usize,
}

/// ドラッグ&ドロップでのファイル/ディレクトリコピー結果。
#[derive(Debug, Clone, Serialize)]
pub struct CopySummary {
    /// 成功したソース数（ディレクトリは 1 として数える）。
    pub copied: usize,
    /// 失敗したソース（パス + 理由）。
    pub failed: Vec<CopyFailure>,
    /// コピー先（ルート相対）。フロントが tree refresh のキーに使う。
    pub dest_rel: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CopyFailure {
    pub source: String,
    pub error: String,
}
