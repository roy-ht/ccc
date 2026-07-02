/** Explorer のプレビュー切替で使うファイル種別判定。バックエンドの判定と揃える。 */

export type FileCategory = "markdown" | "image" | "pdf" | "text" | "binary";

const EXT_MD = new Set(["md", "mdx", "markdown"]);
const EXT_IMAGE = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico"]);
const EXT_PDF = new Set(["pdf"]);
const EXT_BIN = new Set([
  "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib",
  "class", "jar", "war", "wasm", "mp4", "mov", "avi", "mkv", "mp3", "wav",
  "flac", "ogg", "ttf", "otf", "woff", "woff2",
]);

/**
 * 拡張子だけで分類する。サーバ側が最終判定（NUL検出など）するため
 * `text` の中身が実はバイナリということはあり得るが、UI 用の初期分類としては十分。
 */
export function classifyByExt(name: string): FileCategory {
  const dot = name.lastIndexOf(".");
  if (dot < 0 || dot === name.length - 1) return "text";
  const ext = name.slice(dot + 1).toLowerCase();
  if (EXT_MD.has(ext)) return "markdown";
  if (EXT_IMAGE.has(ext)) return "image";
  if (EXT_PDF.has(ext)) return "pdf";
  if (EXT_BIN.has(ext)) return "binary";
  return "text";
}

/** バイト数を人間向け表記に丸める（1.2 MB 等）。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/** 拡張子からアイコン用のカテゴリキー（lucide icon 名にマップする）。 */
export function iconNameForFile(name: string, isDir: boolean): string {
  if (isDir) return "folder";
  const cat = classifyByExt(name);
  switch (cat) {
    case "markdown":
      return "file-text";
    case "image":
      return "image";
    case "pdf":
      return "file-text"; // pdf 専用アイコンは省略
    case "binary":
      return "file-archive";
    default:
      return "file";
  }
}
