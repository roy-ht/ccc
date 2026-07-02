import { formatBytes } from "../../../utils/filetype";

interface Props {
  kind: "binary" | "too-large";
  size: number;
  limit?: number;
  mime?: string | null;
}

/** バイナリ / 大きすぎてプレビューしないファイル用の表示。 */
export function UnsupportedPreview({ kind, size, limit, mime }: Props) {
  return (
    <div className="unsupported-preview">
      {kind === "binary" ? (
        <>
          <div className="unsupported-title">バイナリのためプレビューできません</div>
          <div className="unsupported-info">
            サイズ: {formatBytes(size)}
            {mime && <> · {mime}</>}
          </div>
        </>
      ) : (
        <>
          <div className="unsupported-title">サイズが上限を超えています</div>
          <div className="unsupported-info">
            サイズ: {formatBytes(size)} / 上限: {limit != null ? formatBytes(limit) : "—"}
          </div>
        </>
      )}
    </div>
  );
}
