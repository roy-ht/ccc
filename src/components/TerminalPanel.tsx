import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import "@xterm/xterm/css/xterm.css";
import { InstanceId } from "../types";
import {
  TERMINAL_LINE_HEIGHT,
  TERMINAL_LETTER_SPACING,
  TERMINAL_SCROLLBACK,
  resolveTerminalTheme,
} from "../constants";
import { resolveXtermFontFamilyCached } from "../utils/terminalFont";

interface Props {
  instanceId: InstanceId;
  isVisible: boolean;
  fontFamily: string;
  fontSize: number;
  /** テーマ ID (constants の TERMINAL_THEMES のキー)。未知 ID は default にフォールバック。 */
  colorTheme: string;
  /**
   * reconnect 完了ごとに +1 される値。バックエンドの PTY が新規生成され
   * default 80x24 になってしまうため、変化時に現在の xterm サイズを再 push する。
   */
  reconnectEpoch: number;
  /**
   * WebGL レンダラを使うか。default true。
   * 同一 WKWebView 上で WebGL canvas が複数あるとウィンドウサイズによって描画破綻する
   * 既存の問題（v0.8.1 以前から）があり、Shell 用は false を渡して DOM レンダラに倒す。
   * key 変更を跨いだ動的切替は想定しない（mount 時に 1 度だけ反映）。
   */
  useWebgl?: boolean;
  onData: (instanceId: InstanceId, data: Uint8Array) => void;
  onResize: (instanceId: InstanceId, rows: number, cols: number) => void;
  onReady: (instanceId: InstanceId, writeOutput: (data: Uint8Array) => void) => () => void;
}

export function TerminalPanel({ instanceId, isVisible, fontFamily, fontSize, colorTheme, reconnectEpoch, useWebgl = true, onData, onResize, onReady }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  // WebGL 利用時のみセット。リサイズ後に glyph atlas を焼き直すために保持する。
  const webglAddonRef = useRef<WebglAddon | null>(null);
  const needsInitialResizeRef = useRef(true);

  // コールバックを ref で保持し、Terminal 再作成なしで最新の参照を維持
  const onDataRef = useRef(onData);
  const onResizeRef = useRef(onResize);
  const onReadyRef = useRef(onReady);
  useEffect(() => { onDataRef.current = onData; }, [onData]);
  useEffect(() => { onResizeRef.current = onResize; }, [onResize]);
  useEffect(() => { onReadyRef.current = onReady; }, [onReady]);

  useEffect(() => {
    if (!containerRef.current) return;
    let disposed = false;
    let teardown: (() => void) | null = null;

    // WKWebView の canvas はユーザーフォントを解決できないため、必要なら
    // FontFace 注入を済ませてから Terminal を生成する（terminalFont.ts 参照）。
    // 解決結果はキャッシュされるので 2 回目以降のマウントは同期的に進む。
    const setup = (xtermFontFamily: string) => {
      if (disposed || !containerRef.current) return;

      const term = new Terminal({
        theme: resolveTerminalTheme(colorTheme),
        fontFamily: xtermFontFamily,
        fontSize,
        lineHeight: TERMINAL_LINE_HEIGHT,
        letterSpacing: TERMINAL_LETTER_SPACING,
        cursorBlink: true,
        scrollback: TERMINAL_SCROLLBACK,
        // tmux client は attach/redraw 時に \x1b[2J で画面をクリアする。
        // この際に viewport の内容を scrollback に push することで、
        // 描画前の履歴が失われずマウスホイールで参照できるようにする (PuTTY 互換挙動)。
        scrollOnEraseInDisplay: true,
        // ①②③ 等の曖昧幅文字（フォントは全角グリフ・セル幅は1）を、隣のセルに
        // はみ出させず1セル内に縮小して完全に描画する。幅を2に変える方式は
        // claude code / tmux 側の幅計算（1セル）と食い違い、カーソル制御が密な
        // 入力欄が崩れるため採用しない。WebGL レンダラでのみ有効。
        rescaleOverlappingGlyphs: true,
        // term.unicode（Unicode 11 幅テーブルの有効化）に必要
        allowProposedApi: true,
      });

      // 幅テーブルを Unicode 11 にする。既定の Unicode 6 テーブルは ⭐(U+2B50) や
      // 📁 等の絵文字を1セルと誤計算し、2セルで計算する tmux / claude code と
      // 食い違って表示が崩れる。Unicode 11 はチェーン全体（UAX #11 準拠の
      // tmux / string-width / unicode-width）と一致する。曖昧幅（①等）は
      // 1セルのままで、上の rescaleOverlappingGlyphs と整合する。
      term.loadAddon(new Unicode11Addon());
      term.unicode.activeVersion = "11";

      const fitAddon = new FitAddon();
      term.loadAddon(fitAddon);
      term.open(containerRef.current);

      // GPU レンダラ（rescaleOverlappingGlyphs に必須。DOM レンダラでは効かない）。
      // WebGL コンテキストが取れない/失われた場合は DOM レンダラに戻して継続する。
      // useWebgl=false が渡されている時は明示的に DOM レンダラに倒す（Shell タブ用）。
      if (useWebgl) {
        try {
          const webglAddon = new WebglAddon();
          webglAddon.onContextLoss(() => webglAddon.dispose());
          term.loadAddon(webglAddon);
          webglAddonRef.current = webglAddon;
        } catch (e) {
          console.warn("[ccc] WebGL レンダラの初期化に失敗（DOM レンダラで継続）:", e);
        }
      }

      requestAnimationFrame(() => { fitAddon.fit(); });

      // キーボード入力 → 親コンポーネントへ
      const encoder = new TextEncoder();
      term.onData((str) => {
        onDataRef.current(instanceId, encoder.encode(str));
      });

      // リサイズ → 親コンポーネントへ
      term.onResize(({ rows, cols }) => {
        onResizeRef.current(instanceId, rows, cols);
      });

      // OSC 52 (クリップボード書き込み) ハンドラを登録する。
      // `set-clipboard on` + `terminal-features ',*:clipboard'` により tmux が
      // pane 内アプリの OSC 52 を client_tty まで素通しする経路を受ける。
      // 入力 data は `<targets>;<base64>` 形式 (targets 例: "c"=clipboard,
      // "p"=primary)。base64 ペイロードは UTF-8 バイト列なので、`atob` の結果
      // (Latin-1 文字列) を Uint8Array に詰め直してから TextDecoder で UTF-8 復号する。
      // `atob` のまま使うと "あ" (E3 81 82) が "ã" (E3 を Latin-1 解釈) に化ける。
      const utf8Decoder = new TextDecoder("utf-8");
      term.parser.registerOscHandler(52, (data) => {
        const semi = data.indexOf(";");
        if (semi < 0) return false;
        const payload = data.slice(semi + 1).trim();
        if (!payload) return false;
        let text: string;
        try {
          const bin = atob(payload);
          const bytes = new Uint8Array(bin.length);
          for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
          text = utf8Decoder.decode(bytes);
        } catch {
          return false;
        }
        // 直前のマウス選択を user gesture として navigator.clipboard が通る前提。
        // 失敗時は静かに諦める（Tauri 環境差で permission が無い場合への保険）。
        navigator.clipboard?.writeText(text).catch(() => {});
        return true;
      });

      termRef.current = term;
      fitAddonRef.current = fitAddon;

      // PTY出力 → xterm.js に書き込む関数を登録（バイナリ直接渡し）
      // 初回データ受信時に擬似リサイズを発火し、PTY サイズをウィンドウに同期させる。
      // reconnect 時はバックエンドの PTY が新規生成され default 80x24 になるため、
      // reconnectEpoch 変更時にも同フラグを true に戻して再同期させる。
      needsInitialResizeRef.current = true;
      const writeOutput = (data: Uint8Array) => {
        term.write(data);
        if (needsInitialResizeRef.current) {
          needsInitialResizeRef.current = false;
          requestAnimationFrame(() => {
            fitAddon.fit();
            onResizeRef.current(instanceId, term.rows, term.cols);
          });
        }
      };
      const cleanup = onReadyRef.current(instanceId, writeOutput);

      teardown = () => {
        cleanup();
        webglAddonRef.current = null;
        term.dispose();
      };
    };

    resolveXtermFontFamilyCached(fontFamily).then(setup);

    return () => {
      disposed = true;
      teardown?.();
    };
  }, [instanceId, fontFamily, fontSize, colorTheme]);

  // ウィンドウリサイズ追従
  // マウスドラッグ中は ResizeObserver が毎フレーム発火する。連続呼び出しは
  // PTY/tmux/claude の SIGWINCH 伝播が追いつかず描画破綻するので 120ms trailing debounce。
  //
  // WebGL レンダラは scrollback が多い状態（claude code は alternate screen 不使用設計のため
  // 全画面 redraw がそのまま scrollback に積まれる）で resize 時に glyph atlas / canvas
  // 内部解像度の追従に失敗する既知の症状がある。clearTextureAtlas() 単発では足りないので
  // WebglAddon を dispose → 新規 attach して内部 state をリセットする。
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const observer = new ResizeObserver(() => {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        const term = termRef.current;
        if (!term) return;
        fitAddonRef.current?.fit();

        if (useWebgl && webglAddonRef.current) {
          try {
            webglAddonRef.current.dispose();
          } catch {
            // すでに dispose 済み等は無視
          }
          webglAddonRef.current = null;
          try {
            const next = new WebglAddon();
            next.onContextLoss(() => next.dispose());
            term.loadAddon(next);
            webglAddonRef.current = next;
          } catch (e) {
            console.warn("[ccc] WebGL レンダラの再 attach に失敗（DOM レンダラで継続）:", e);
          }
        }
        term.refresh(0, term.rows - 1);
      }, 120);
    });
    if (containerRef.current) {
      observer.observe(containerRef.current);
    }
    return () => {
      observer.disconnect();
      if (timer !== null) clearTimeout(timer);
    };
  }, [useWebgl]);

  // 表示時にフィット再計算してフォーカス（ダブルRAFでレイアウト確定を待つ）
  useEffect(() => {
    if (!isVisible) return;
    let id = requestAnimationFrame(() => {
      id = requestAnimationFrame(() => {
        fitAddonRef.current?.fit();
        termRef.current?.focus();
      });
    });
    return () => cancelAnimationFrame(id);
  }, [isVisible]);

  // reconnect 完了時の再同期。新規 PTY は default 80x24 で生成されるため、
  // バックエンド側の last_size 適用に加えて、フロントからも現サイズを再 push する。
  // 初回マウント時 (reconnectEpoch === 0) はスキップ。
  useEffect(() => {
    if (reconnectEpoch === 0) return;
    needsInitialResizeRef.current = true;
    const term = termRef.current;
    const fit = fitAddonRef.current;
    if (!term || !fit) return;
    requestAnimationFrame(() => {
      fit.fit();
      onResizeRef.current(instanceId, term.rows, term.cols);
    });
  }, [reconnectEpoch, instanceId]);

  return (
    <div
      ref={containerRef}
      className="terminal-container"
      style={{ display: isVisible ? "flex" : "none" }}
    />
  );
}
