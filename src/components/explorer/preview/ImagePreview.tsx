import { formatBytes } from "../../../utils/filetype";

interface Props {
  mime: string;
  base64: string;
  size: number;
}

/** 画像プレビュー（base64 を data URL で `<img>` に渡す）。 */
export function ImagePreview({ mime, base64, size }: Props) {
  const src = `data:${mime};base64,${base64}`;
  return (
    <div className="image-preview">
      <div className="image-preview-meta">
        {mime} · {formatBytes(size)}
      </div>
      <div className="image-preview-body">
        <img src={src} alt="" />
      </div>
    </div>
  );
}
