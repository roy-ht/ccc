//! メモリ資産（`CLAUDE.md` / `memory/` / `rules/`）のスナップショットと版管理。
//!
//! Claude Code のメモリは 30 日で揮発し、リモートのマシンごと破棄され得る。これを
//! `memory_snapshots` に**内容ハッシュで重複排除しつつバージョン付き**で保存し、揮発・
//! 上書きから守る。
//!
//! 走査対象（`CLAUDE_CONFIG_DIR`＝プロファイルディレクトリ相対）:
//! - `CLAUDE.md`              … プロファイル直下のユーザーメモリ（scope=user）
//! - `rules/**/*.md`          … グローバル規約（scope=user）
//! - `projects/*/MEMORY.md`   … プロジェクトメモリ索引（scope=project）
//! - `projects/*/memory/**/*.md` … 個別ファクト（scope=project）
//!
//! `todos/` `plans/` 等の揮発的作業状態は対象外。リモート pull のステージング
//! （`~/.ccc/archive/pulled/<host>/<profile>/`）も同レイアウトなので同じ関数で扱える
//! （`source_kind="remote"` / `host_alias` を渡す。Phase 3 が利用）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// スナップショット走査の結果。
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryStats {
    /// 走査した（読み取れた）ファイル数。
    pub scanned: usize,
    /// 内容が変わって新版として保存した数（重複は除く）。
    pub new_versions: usize,
}

/// `list_memory` の最新版 1 行。
#[derive(Debug, Serialize)]
pub struct MemoryEntry {
    pub agent_profile: Option<String>,
    pub rel_path: String,
    pub scope: Option<String>,
    pub project: Option<String>,
    pub source_kind: Option<String>,
    pub host_alias: Option<String>,
    pub content_hash: Option<String>,
    pub captured_at: Option<i64>,
    /// この (agent_profile, rel_path) に蓄積された版数。
    pub versions: i64,
}

/// メモリ版 1 件（`show` / `diff` / `restore` 用。`content` を含む）。
#[derive(Debug, Serialize)]
pub struct MemoryVersion {
    pub id: i64,
    pub agent_profile: Option<String>,
    pub rel_path: String,
    pub scope: Option<String>,
    pub project: Option<String>,
    pub content_hash: Option<String>,
    pub source_kind: Option<String>,
    pub host_alias: Option<String>,
    pub captured_at: Option<i64>,
    pub content: Option<String>,
}

/// `list_memory` のフィルタ。`None` は無視。
#[derive(Debug, Default)]
pub struct MemoryFilter {
    pub profile: Option<String>,
    pub scope: Option<String>,
    pub project: Option<String>,
}

/// プロファイルディレクトリ配下のメモリ資産を走査し、内容が変わったものだけ
/// 新版として保存する（`UNIQUE(agent_profile, rel_path, content_hash)` で重複排除）。
pub fn snapshot_memory(
    conn: &Connection,
    profile_dir: &Path,
    agent_profile: &str,
    source_kind: &str,
    host_alias: Option<&str>,
) -> anyhow::Result<MemoryStats> {
    let mut stats = MemoryStats::default();
    if !profile_dir.is_dir() {
        return Ok(stats);
    }
    let now = now_ms();

    for t in collect_targets(profile_dir)? {
        let content = match std::fs::read_to_string(&t.path) {
            Ok(c) => c,
            Err(_) => continue, // 読めない/非 UTF-8 はスキップ
        };
        stats.scanned += 1;
        let rel_path = t
            .path
            .strip_prefix(profile_dir)
            .unwrap_or(&t.path)
            .to_string_lossy()
            .replace('\\', "/");
        let hash = sha256_hex(content.as_bytes());
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO memory_snapshots
               (agent_profile, rel_path, scope, project, content_hash, content,
                source_kind, host_alias, captured_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                agent_profile,
                rel_path,
                t.scope,
                t.project,
                hash,
                content,
                source_kind,
                host_alias,
                now,
            ],
        )?;
        if inserted > 0 {
            stats.new_versions += 1;
        }
    }
    Ok(stats)
}

/// 最新版を `(agent_profile, rel_path)` ごとに 1 行返す（活動の新しい順）。
pub fn list_memory(conn: &Connection, f: &MemoryFilter) -> anyhow::Result<Vec<MemoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT a.agent_profile, a.rel_path, a.scope, a.project, a.source_kind, a.host_alias,
                a.content_hash, a.captured_at,
                (SELECT COUNT(*) FROM memory_snapshots b
                   WHERE COALESCE(b.agent_profile,'') = COALESCE(a.agent_profile,'')
                     AND b.rel_path = a.rel_path) AS versions
         FROM memory_snapshots a
         WHERE a.id = (
             SELECT c.id FROM memory_snapshots c
             WHERE COALESCE(c.agent_profile,'') = COALESCE(a.agent_profile,'')
               AND c.rel_path = a.rel_path
             ORDER BY c.captured_at DESC, c.id DESC LIMIT 1)
           AND (?1 IS NULL OR a.agent_profile = ?1)
           AND (?2 IS NULL OR a.scope = ?2)
           AND (?3 IS NULL OR a.project = ?3)
         ORDER BY a.captured_at DESC, a.rel_path",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![f.profile, f.scope, f.project], |r| {
            Ok(MemoryEntry {
                agent_profile: r.get(0)?,
                rel_path: r.get(1)?,
                scope: r.get(2)?,
                project: r.get(3)?,
                source_kind: r.get(4)?,
                host_alias: r.get(5)?,
                content_hash: r.get(6)?,
                captured_at: r.get(7)?,
                versions: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 指定 `rel_path` の全版を新しい順に返す。`profile` を渡せば絞り込む。
pub fn memory_versions(
    conn: &Connection,
    profile: Option<&str>,
    rel_path: &str,
) -> anyhow::Result<Vec<MemoryVersion>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_profile, rel_path, scope, project, content_hash,
                source_kind, host_alias, captured_at, content
         FROM memory_snapshots
         WHERE rel_path = ?1 AND (?2 IS NULL OR agent_profile = ?2)
         ORDER BY captured_at DESC, id DESC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![rel_path, profile], |r| {
            Ok(MemoryVersion {
                id: r.get(0)?,
                agent_profile: r.get(1)?,
                rel_path: r.get(2)?,
                scope: r.get(3)?,
                project: r.get(4)?,
                content_hash: r.get(5)?,
                source_kind: r.get(6)?,
                host_alias: r.get(7)?,
                captured_at: r.get(8)?,
                content: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// インスタンス（プロファイル＋当該プロジェクト）に紐づくメモリの最新版を返す。
///
/// - user scope（`CLAUDE.md` / `rules/**`）は当該 `agent_profile` の全件
/// - project scope は `encoded_cwd`（`projects/<encoded-cwd>/...`）に一致するファイルのみ
/// - `query` 指定時は rel_path / content の部分一致でさらに絞り込む（横断検索ではなく
///   このインスタンスのメモリ内に閉じた絞り込み）
pub fn list_memory_for_instance(
    conn: &Connection,
    agent_profile: &str,
    encoded_cwd: &str,
    query: Option<&str>,
) -> anyhow::Result<Vec<MemoryEntry>> {
    let proj_like = format!("projects/{encoded_cwd}/%");
    let q_like = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| format!("%{}%", escape_like(q)));

    let mut sql = String::from(
        "SELECT a.agent_profile, a.rel_path, a.scope, a.project, a.source_kind, a.host_alias,
                a.content_hash, a.captured_at,
                (SELECT COUNT(*) FROM memory_snapshots b
                   WHERE COALESCE(b.agent_profile,'') = COALESCE(a.agent_profile,'')
                     AND b.rel_path = a.rel_path) AS versions
         FROM memory_snapshots a
         WHERE a.id = (
             SELECT c.id FROM memory_snapshots c
             WHERE COALESCE(c.agent_profile,'') = COALESCE(a.agent_profile,'')
               AND c.rel_path = a.rel_path
             ORDER BY c.captured_at DESC, c.id DESC LIMIT 1)
           AND a.agent_profile = ?1
           AND (a.scope = 'user' OR a.rel_path LIKE ?2)",
    );
    if q_like.is_some() {
        sql.push_str(" AND (a.rel_path LIKE ?3 ESCAPE '\\' OR a.content LIKE ?3 ESCAPE '\\')");
    }
    sql.push_str(" ORDER BY a.captured_at DESC, a.rel_path");

    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<MemoryEntry> {
        Ok(MemoryEntry {
            agent_profile: r.get(0)?,
            rel_path: r.get(1)?,
            scope: r.get(2)?,
            project: r.get(3)?,
            source_kind: r.get(4)?,
            host_alias: r.get(5)?,
            content_hash: r.get(6)?,
            captured_at: r.get(7)?,
            versions: r.get(8)?,
        })
    };
    let rows = match &q_like {
        Some(q) => stmt
            .query_map(rusqlite::params![agent_profile, proj_like, q], map)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(rusqlite::params![agent_profile, proj_like], map)?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

/// 指定 `rel_path` の最新版本文を返す（無ければ `None`）。
pub fn memory_latest_content(
    conn: &Connection,
    agent_profile: &str,
    rel_path: &str,
) -> anyhow::Result<Option<String>> {
    let content = conn
        .query_row(
            "SELECT content FROM memory_snapshots
             WHERE agent_profile = ?1 AND rel_path = ?2
             ORDER BY captured_at DESC, id DESC LIMIT 1",
            rusqlite::params![agent_profile, rel_path],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(content)
}

/// LIKE のメタ文字（`%` `_` `\`）をエスケープする（`ESCAPE '\'` と併用）。
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 2 つのテキストの行単位差分を `+`/`-` 行で返す（共通行は省く）。
///
/// LCS（最長共通部分列）で対応を取る。メモリファイルは小さいので O(n·m) で十分。
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = a[i..] と b[j..] の LCS 長
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push_str("- ");
            out.push_str(a[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push_str("+ ");
            out.push_str(b[j]);
            out.push('\n');
            j += 1;
        }
    }
    for line in &a[i..] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &b[j..] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ── 内部 ──

/// スナップショット対象 1 ファイル。
struct Target {
    path: PathBuf,
    scope: &'static str,
    project: Option<String>,
}

/// プロファイルディレクトリから対象ファイル群を収集する。
fn collect_targets(profile_dir: &Path) -> anyhow::Result<Vec<Target>> {
    let mut targets = Vec::new();

    // CLAUDE.md（プロファイル直下のユーザーメモリ）
    let claude_md = profile_dir.join("CLAUDE.md");
    if claude_md.is_file() {
        targets.push(Target {
            path: claude_md,
            scope: "user",
            project: None,
        });
    }
    // rules/**/*.md（グローバル規約）
    let rules = profile_dir.join("rules");
    if rules.is_dir() {
        for p in collect_md(&rules)? {
            targets.push(Target {
                path: p,
                scope: "user",
                project: None,
            });
        }
    }
    // projects/*/{MEMORY.md, memory/**/*.md}
    let projects = profile_dir.join("projects");
    if projects.is_dir() {
        for proj in std::fs::read_dir(&projects)?.flatten() {
            let pdir = proj.path();
            if !pdir.is_dir() {
                continue;
            }
            let project = pdir.file_name().and_then(|s| s.to_str()).map(project_label);
            let index = pdir.join("MEMORY.md");
            if index.is_file() {
                targets.push(Target {
                    path: index,
                    scope: "project",
                    project: project.clone(),
                });
            }
            let mem_dir = pdir.join("memory");
            if mem_dir.is_dir() {
                for p in collect_md(&mem_dir)? {
                    targets.push(Target {
                        path: p,
                        scope: "project",
                        project: project.clone(),
                    });
                }
            }
        }
    }
    Ok(targets)
}

/// ディレクトリ以下の `*.md` を再帰収集する。
///
/// `DirEntry::file_type` はシンボリックリンクを辿らないため、symlink 先のディレクトリは
/// `is_dir()==false` で自然にスキップされる（安全側）。
fn collect_md(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)?.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 符号化された `projects/<encoded-cwd>` 名から表示用の短縮プロジェクト名を導く。
/// 符号化（`/`・`.`・`-` を全て `-` 化）は復元不能なので、末尾セグメントを採用する
/// （例: `-Users-user-mydocs-ccc` → `ccc`）。一意性は `rel_path` が担保する。
fn project_label(encoded: &str) -> String {
    encoded
        .rsplit('-')
        .find(|s| !s.is_empty())
        .unwrap_or(encoded)
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// プロファイルディレクトリを temp に組み立てる。
    fn tmp_profile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-mem-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn snapshot_collects_all_scopes_and_dedups() {
        let dir = tmp_profile("collect");
        write_file(&dir.join("CLAUDE.md"), "# ユーザーメモリ\n方針A");
        write_file(&dir.join("rules/base.md"), "規約1");
        write_file(&dir.join("projects/-Users-h-mydocs-ccc/MEMORY.md"), "索引");
        write_file(
            &dir.join("projects/-Users-h-mydocs-ccc/memory/fact.md"),
            "事実1",
        );
        // 対象外（.md でない / 揮発作業状態）
        write_file(&dir.join("todos/x.json"), "{}");
        write_file(&dir.join("projects/-Users-h-mydocs-ccc/notes.txt"), "txt");

        let conn = crate::open_in_memory().unwrap();
        let s = snapshot_memory(&conn, &dir, "max_plan", "local", None).unwrap();
        assert_eq!(s.scanned, 4, "CLAUDE.md + rules + MEMORY.md + memory/fact");
        assert_eq!(s.new_versions, 4);

        // 再走査（無変更）→ 新版 0
        let s2 = snapshot_memory(&conn, &dir, "max_plan", "local", None).unwrap();
        assert_eq!(s2.new_versions, 0);

        // scope と project が入っている
        let entries = list_memory(&conn, &MemoryFilter::default()).unwrap();
        assert_eq!(entries.len(), 4);
        let project_mem = entries
            .iter()
            .find(|e| e.rel_path.ends_with("memory/fact.md"))
            .unwrap();
        assert_eq!(project_mem.scope.as_deref(), Some("project"));
        assert_eq!(project_mem.project.as_deref(), Some("ccc"));
        let claude = entries.iter().find(|e| e.rel_path == "CLAUDE.md").unwrap();
        assert_eq!(claude.scope.as_deref(), Some("user"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_change_creates_new_version() {
        let dir = tmp_profile("version");
        let claude = dir.join("CLAUDE.md");
        write_file(&claude, "v1");
        let conn = crate::open_in_memory().unwrap();
        snapshot_memory(&conn, &dir, "p", "local", None).unwrap();

        write_file(&claude, "v2");
        let s = snapshot_memory(&conn, &dir, "p", "local", None).unwrap();
        assert_eq!(s.new_versions, 1, "内容変化で新版");

        let versions = memory_versions(&conn, Some("p"), "CLAUDE.md").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].content.as_deref(), Some("v2"), "新しい順");
        assert_eq!(versions[1].content.as_deref(), Some("v1"));

        let entries = list_memory(&conn, &MemoryFilter::default()).unwrap();
        assert_eq!(entries[0].versions, 2);
        assert_eq!(entries[0].content_hash, versions[0].content_hash);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_filter_by_scope() {
        let dir = tmp_profile("filter");
        write_file(&dir.join("CLAUDE.md"), "u");
        write_file(&dir.join("projects/-a-b/MEMORY.md"), "p");
        let conn = crate::open_in_memory().unwrap();
        snapshot_memory(&conn, &dir, "p", "local", None).unwrap();

        let user = list_memory(
            &conn,
            &MemoryFilter {
                scope: Some("user".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].rel_path, "CLAUDE.md");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_for_instance_scopes_profile_and_project() {
        let conn = crate::open_in_memory().unwrap();
        let ins = |profile: &str,
                   rel: &str,
                   scope: &str,
                   project: Option<&str>,
                   hash: &str,
                   content: &str,
                   at: i64| {
            conn.execute(
                "INSERT INTO memory_snapshots(agent_profile, rel_path, scope, project, content_hash, content, captured_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![profile, rel, scope, project, hash, content, at],
            )
            .unwrap();
        };
        // user scope（profile=p）
        ins("p", "CLAUDE.md", "user", None, "h1", "ユーザーメモリ", 100);
        ins("p", "rules/base.md", "user", None, "h2", "規約", 90);
        // project scope: 当該プロジェクト（encoded=-w-ccc）と別プロジェクト
        ins(
            "p",
            "projects/-w-ccc/MEMORY.md",
            "project",
            Some("ccc"),
            "h3",
            "ccc索引",
            110,
        );
        ins(
            "p",
            "projects/-w-other/MEMORY.md",
            "project",
            Some("other"),
            "h4",
            "別索引",
            120,
        );
        // 別プロファイルの user scope は除外
        ins("q", "CLAUDE.md", "user", None, "h5", "別profile", 130);

        let entries = list_memory_for_instance(&conn, "p", "-w-ccc", None).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        // user 2 件＋当該プロジェクト 1 件＝3 件。別プロジェクト・別profileは出ない。
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&"CLAUDE.md"));
        assert!(paths.contains(&"rules/base.md"));
        assert!(paths.contains(&"projects/-w-ccc/MEMORY.md"));
        assert!(!paths.contains(&"projects/-w-other/MEMORY.md"));

        // 新しい順（captured_at DESC）: -w-ccc(110) → CLAUDE(100) → rules(90)
        assert_eq!(entries[0].rel_path, "projects/-w-ccc/MEMORY.md");

        // query 絞り込み（rel_path 一致）
        let q = list_memory_for_instance(&conn, "p", "-w-ccc", Some("base")).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].rel_path, "rules/base.md");

        // query 絞り込み（content 一致）
        let q2 = list_memory_for_instance(&conn, "p", "-w-ccc", Some("索引")).unwrap();
        assert_eq!(q2.len(), 1);
        assert_eq!(q2[0].rel_path, "projects/-w-ccc/MEMORY.md");

        // 最新版本文
        assert_eq!(
            memory_latest_content(&conn, "p", "CLAUDE.md")
                .unwrap()
                .as_deref(),
            Some("ユーザーメモリ")
        );
        assert_eq!(memory_latest_content(&conn, "p", "nope.md").unwrap(), None);
    }

    #[test]
    fn line_diff_marks_changes() {
        let d = line_diff("a\nb\nc", "a\nB\nc");
        assert_eq!(d, "- b\n+ B\n");
        assert_eq!(line_diff("x", "x"), "", "無変更は空");
        assert_eq!(line_diff("", "new"), "+ new\n");
    }
}
