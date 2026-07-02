//! ripgrep の `--json` 出力をパースして `SearchHit` 列を組む。
//! バイナリ判定ヘルパも同居（先頭バイトに NUL を含むかで判別する軽量版）。

use anyhow::{Context, Result};
use serde::Deserialize;

use super::types::SearchHit;

/// 先頭 `head` バイトに NUL を含めばバイナリ扱い。
pub fn is_binary_head(head: &[u8]) -> bool {
    head.contains(&0)
}

/// `rg --json` の標準出力をパースし、`type=="match"` の行を `SearchHit` に変換する。
/// `root` は SearchHit.path を相対化するためのプレフィクス（POSIX）。
///
/// 一致行が 1 件あたり複数の `submatches` を持つ場合は、最初のサブマッチのみを使う。
pub fn parse_rg_json(stdout: &[u8], root: &str, max_results: usize) -> Result<Vec<SearchHit>> {
    let text = std::str::from_utf8(stdout).context("rg 出力が UTF-8 ではありません")?;
    let mut hits = Vec::new();
    for line in text.lines() {
        if hits.len() >= max_results {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let evt: Event = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue, // begin/end/summary など想定外のメッセージはスキップ
        };
        if let Event {
            kind,
            data:
                Some(MatchData {
                    path,
                    line_number,
                    lines,
                    submatches,
                }),
        } = evt
        {
            if kind != "match" {
                continue;
            }
            let abs_path = path.text.unwrap_or_default();
            let rel = relativize(&abs_path, root);
            let line_text = lines.text.unwrap_or_default();
            // 行末改行を落とし、UI 表示用に先頭 300 字へ切り詰める。
            let line_text = line_text.trim_end_matches('\n').to_string();
            let line_text = if line_text.chars().count() > 300 {
                let truncated: String = line_text.chars().take(300).collect();
                format!("{truncated}…")
            } else {
                line_text
            };
            let (start, end) = submatches
                .first()
                .map(|sm| (sm.start, sm.end))
                .unwrap_or((0, 0));
            hits.push(SearchHit {
                path: rel,
                line_number,
                line: line_text,
                match_start: start,
                match_end: end,
            });
        }
    }
    Ok(hits)
}

fn relativize(abs: &str, root: &str) -> String {
    let root_clean = root.trim_end_matches('/');
    if let Some(stripped) = abs.strip_prefix(&format!("{root_clean}/")) {
        stripped.to_string()
    } else if abs == root_clean {
        String::new()
    } else if let Some(stripped) = abs.strip_prefix("./") {
        // `rg --json` をディレクトリ `.` 指定で走らせると `./path` 形式になる
        stripped.to_string()
    } else {
        abs.to_string()
    }
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Option<MatchData>,
}

#[derive(Deserialize)]
struct MatchData {
    path: TextField,
    line_number: u64,
    lines: TextField,
    #[serde(default)]
    submatches: Vec<SubMatch>,
}

#[derive(Deserialize)]
struct TextField {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct SubMatch {
    start: usize,
    end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_head_detects_nul() {
        assert!(is_binary_head(&[1, 2, 0, 4]));
        assert!(!is_binary_head(b"hello world"));
    }

    #[test]
    fn parses_match_lines() {
        // rg --json は begin / match / end / summary の混在を出すが、ここでは match のみを抜く。
        let stdout = r#"{"type":"begin","data":{"path":{"text":"/r/src/main.rs"}}}
{"type":"match","data":{"path":{"text":"/r/src/main.rs"},"lines":{"text":"fn main() {\n"},"line_number":3,"absolute_offset":42,"submatches":[{"match":{"text":"main"},"start":3,"end":7}]}}
{"type":"end","data":{"path":{"text":"/r/src/main.rs"},"binary_offset":null,"stats":{}}}
"#;
        let hits = parse_rg_json(stdout.as_bytes(), "/r", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/main.rs");
        assert_eq!(hits[0].line_number, 3);
        assert_eq!(hits[0].line, "fn main() {");
        assert_eq!(hits[0].match_start, 3);
        assert_eq!(hits[0].match_end, 7);
    }

    #[test]
    fn truncates_to_max() {
        let line = r#"{"type":"match","data":{"path":{"text":"/r/a.txt"},"lines":{"text":"x"},"line_number":1,"submatches":[{"start":0,"end":1}]}}"#;
        let many: String = (0..5).map(|_| line).collect::<Vec<_>>().join("\n");
        let hits = parse_rg_json(many.as_bytes(), "/r", 3).unwrap();
        assert_eq!(hits.len(), 3);
    }
}
