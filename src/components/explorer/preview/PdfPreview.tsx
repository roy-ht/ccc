import { useEffect, useMemo, useState } from "react";
import { Document, Page, pdfjs } from "react-pdf";
import "react-pdf/dist/Page/AnnotationLayer.css";
import "react-pdf/dist/Page/TextLayer.css";
import { formatBytes } from "../../../utils/filetype";

// react-pdf 公式推奨: Worker は Document/Page を使う同じモジュールで設定する。
// `new URL(..., import.meta.url)` パターンは Vite が pdfjs-dist のバージョンと
// 一致した worker ファイルを bundle に取り込んでくれる。
pdfjs.GlobalWorkerOptions.workerSrc = new URL(
  "pdfjs-dist/build/pdf.worker.min.mjs",
  import.meta.url
).toString();

interface Props {
  base64: string;
  size: number;
}

/**
 * PDF プレビュー。`base64` → `Uint8Array` に変換し、react-pdf の Document に渡す。
 * 描画ページは「→ / ←」ボタンで切り替え。
 */
export function PdfPreview({ base64, size }: Props) {
  const [numPages, setNumPages] = useState<number | null>(null);
  const [pageIndex, setPageIndex] = useState(0);
  const [loadError, setLoadError] = useState<string | null>(null);

  // ファイル切替で 1 ページ目に戻す
  useEffect(() => {
    setPageIndex(0);
    setNumPages(null);
    setLoadError(null);
  }, [base64]);

  // base64 → Uint8Array（react-pdf に data として渡す）。
  // file prop はオブジェクト参照が変わると再ロードがかかるため、memo 化する。
  const fileSpec = useMemo(() => ({ data: base64ToUint8Array(base64) }), [base64]);

  return (
    <div className="pdf-preview">
      <div className="pdf-preview-toolbar">
        <button
          className="pdf-preview-btn"
          disabled={pageIndex <= 0}
          onClick={() => setPageIndex((i) => Math.max(0, i - 1))}
        >
          ←
        </button>
        <span className="pdf-preview-info">
          {numPages ? `${pageIndex + 1} / ${numPages}` : "—"} · {formatBytes(size)}
        </span>
        <button
          className="pdf-preview-btn"
          disabled={numPages != null && pageIndex >= numPages - 1}
          onClick={() => setPageIndex((i) => (numPages ? Math.min(numPages - 1, i + 1) : i))}
        >
          →
        </button>
      </div>
      <div className="pdf-preview-body">
        {loadError ? (
          <div className="pdf-preview-error">PDF を表示できません: {loadError}</div>
        ) : (
          <Document
            file={fileSpec}
            onLoadSuccess={({ numPages }) => setNumPages(numPages)}
            onLoadError={(err) => setLoadError(err.message)}
            loading={<div className="pdf-preview-loading">読み込み中…</div>}
            error={<div className="pdf-preview-error">PDF を表示できません</div>}
          >
            <Page pageNumber={pageIndex + 1} width={700} />
          </Document>
        )}
      </div>
    </div>
  );
}

function base64ToUint8Array(b64: string): Uint8Array {
  const bin = atob(b64);
  const len = bin.length;
  const arr = new Uint8Array(len);
  for (let i = 0; i < len; i++) arr[i] = bin.charCodeAt(i);
  return arr;
}
