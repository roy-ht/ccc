import { useEffect, useMemo, useRef, useState } from "react";

interface Props {
  text: string;
  /** 値が増えるたびにタイプ演出を再生する。0 のときは演出しない（初回マウント用） */
  trigger: number;
  /** 全文の表示にかける時間。文字数によらず一定（高速に書き込む感じを保つ） */
  durationMs?: number;
}

/**
 * タイプライター風テキスト。trigger が変わると text を先頭から高速に
 * 打ち込み直す。サイドバーの状態メッセージ演出（地味系）に使う。
 *
 * - 文字単位は code point（Array.from）で刻み、サロゲートペアを壊さない
 * - prefers-reduced-motion では演出せず即時表示
 */
export function TypewriterText({ text, trigger, durationMs = 280 }: Props) {
  const chars = useMemo(() => Array.from(text), [text]);
  const [visible, setVisible] = useState(chars.length);
  const reducedMotion = useRef(
    typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches
  );

  useEffect(() => {
    if (trigger === 0 || reducedMotion.current || chars.length === 0) {
      setVisible(chars.length);
      return;
    }
    setVisible(0);
    const start = performance.now();
    let raf = 0;
    const tick = (now: number) => {
      const p = Math.min(1, (now - start) / durationMs);
      setVisible(Math.ceil(p * chars.length));
      if (p < 1) {
        raf = requestAnimationFrame(tick);
      }
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [trigger, chars, durationMs]);

  return <>{chars.slice(0, visible).join("")}</>;
}

/**
 * 値の変化を数えるフック。初回マウントは 0 を返し、値が変わるたびに増える。
 * `TypewriterText` の trigger に渡す用途（初回は演出させない）。
 */
export function useChangeSeq<T>(value: T): number {
  const ref = useRef<{ value: T; seq: number }>({ value, seq: 0 });
  if (!Object.is(ref.current.value, value)) {
    ref.current = { value, seq: ref.current.seq + 1 };
  }
  return ref.current.seq;
}
