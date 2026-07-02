import { TerminalTheme } from "./types";

// Terminal 設定
export const TERMINAL_LINE_HEIGHT = 1.0;
// WebGL レンダラではこの値が device px 単位の整数セル幅演算に使われる。
// 負値はセル幅を奇数 device px にし、グリフの隣セルはみ出しと
// canvas 全体の非整数スケール表示（にじみ）を引き起こすため 0 とする。
export const TERMINAL_LETTER_SPACING = 0;
// xterm.js 側の scrollback は 0 にして「常に全画面 fit、スクロールバーなし」設計にする。
// scrollback を持つと viewport の高さが rows × cellHeight を超えてブラウザ標準の
// スクロールバーが出現し、container の content width が縮む → ResizeObserver 再発火 →
// 描画破綻、というループに入る。履歴ナビゲーションは tmux 側の copy-mode に移譲する
// （consts.rs の `set -g mouse on` で wheel を tmux に渡す）。
export const TERMINAL_SCROLLBACK = 0;

// ─── テーマプリセット ────────────────────────────────────────────────────────

export interface TerminalThemePreset {
  id: string;
  label: string;
  theme: TerminalTheme;
}

/**
 * テーマ ID → プリセット。設定 UI / TerminalPanel の双方から参照する。
 * 未知の ID が渡された場合は `DEFAULT_TERMINAL_THEME_ID` のテーマにフォールバック。
 */
export const TERMINAL_THEMES: Record<string, TerminalThemePreset> = {
  default: {
    id: "default",
    label: "Default (Dark)",
    theme: {
      background: "#0f1117",
      foreground: "#e2e8f0",
      cursor: "#60a5fa",
      selectionBackground: "#3b82f640",
      black: "#1e293b",
      red: "#ef4444",
      green: "#22c55e",
      yellow: "#eab308",
      blue: "#3b82f6",
      magenta: "#ec4899",
      cyan: "#06b6d4",
      white: "#e2e8f0",
      brightBlack: "#475569",
      brightRed: "#f87171",
      brightGreen: "#4ade80",
      brightYellow: "#facc15",
      brightBlue: "#60a5fa",
      brightMagenta: "#f472b6",
      brightCyan: "#22d3ee",
      brightWhite: "#f1f5f9",
    },
  },
  "github-dark": {
    id: "github-dark",
    label: "GitHub Dark",
    theme: {
      background: "#0d1117",
      foreground: "#c9d1d9",
      cursor: "#c9d1d9",
      selectionBackground: "#388bfd66",
      black: "#484f58",
      red: "#ff7b72",
      green: "#3fb950",
      yellow: "#d29922",
      blue: "#58a6ff",
      magenta: "#bc8cff",
      cyan: "#39c5cf",
      white: "#b1bac4",
      brightBlack: "#6e7681",
      brightRed: "#ffa198",
      brightGreen: "#56d364",
      brightYellow: "#e3b341",
      brightBlue: "#79c0ff",
      brightMagenta: "#d2a8ff",
      brightCyan: "#56d4dd",
      brightWhite: "#ffffff",
    },
  },
  "catppuccin-mocha": {
    id: "catppuccin-mocha",
    label: "Catppuccin Mocha",
    theme: {
      background: "#1e1e2e",
      foreground: "#cdd6f4",
      cursor: "#f5e0dc",
      selectionBackground: "#585b7066",
      black: "#45475a",
      red: "#f38ba8",
      green: "#a6e3a1",
      yellow: "#f9e2af",
      blue: "#89b4fa",
      magenta: "#f5c2e7",
      cyan: "#94e2d5",
      white: "#bac2de",
      brightBlack: "#585b70",
      brightRed: "#f38ba8",
      brightGreen: "#a6e3a1",
      brightYellow: "#f9e2af",
      brightBlue: "#89b4fa",
      brightMagenta: "#f5c2e7",
      brightCyan: "#94e2d5",
      brightWhite: "#a6adc8",
    },
  },
  "catppuccin-macchiato": {
    id: "catppuccin-macchiato",
    label: "Catppuccin Macchiato",
    theme: {
      background: "#24273a",
      foreground: "#cad3f5",
      cursor: "#f4dbd6",
      selectionBackground: "#5b607866",
      black: "#494d64",
      red: "#ed8796",
      green: "#a6da95",
      yellow: "#eed49f",
      blue: "#8aadf4",
      magenta: "#f5bde6",
      cyan: "#8bd5ca",
      white: "#b8c0e0",
      brightBlack: "#5b6078",
      brightRed: "#ed8796",
      brightGreen: "#a6da95",
      brightYellow: "#eed49f",
      brightBlue: "#8aadf4",
      brightMagenta: "#f5bde6",
      brightCyan: "#8bd5ca",
      brightWhite: "#a5adcb",
    },
  },
  "catppuccin-frappe": {
    id: "catppuccin-frappe",
    label: "Catppuccin Frappé",
    theme: {
      background: "#303446",
      foreground: "#c6d0f5",
      cursor: "#f2d5cf",
      selectionBackground: "#62688066",
      black: "#51576d",
      red: "#e78284",
      green: "#a6d189",
      yellow: "#e5c890",
      blue: "#8caaee",
      magenta: "#f4b8e4",
      cyan: "#81c8be",
      white: "#b5bfe2",
      brightBlack: "#626880",
      brightRed: "#e78284",
      brightGreen: "#a6d189",
      brightYellow: "#e5c890",
      brightBlue: "#8caaee",
      brightMagenta: "#f4b8e4",
      brightCyan: "#81c8be",
      brightWhite: "#a5adce",
    },
  },
  "catppuccin-latte": {
    id: "catppuccin-latte",
    label: "Catppuccin Latte",
    theme: {
      background: "#eff1f5",
      foreground: "#4c4f69",
      cursor: "#dc8a78",
      selectionBackground: "#acb0be66",
      black: "#bcc0cc",
      red: "#d20f39",
      green: "#40a02b",
      yellow: "#df8e1d",
      blue: "#1e66f5",
      magenta: "#ea76cb",
      cyan: "#179299",
      white: "#5c5f77",
      brightBlack: "#acb0be",
      brightRed: "#d20f39",
      brightGreen: "#40a02b",
      brightYellow: "#df8e1d",
      brightBlue: "#1e66f5",
      brightMagenta: "#ea76cb",
      brightCyan: "#179299",
      brightWhite: "#6c6f85",
    },
  },
};

export const DEFAULT_TERMINAL_THEME_ID = "default";

/** 未知 ID は default にフォールバック。 */
export function resolveTerminalTheme(id: string): TerminalTheme {
  return (TERMINAL_THEMES[id] ?? TERMINAL_THEMES[DEFAULT_TERMINAL_THEME_ID]).theme;
}
