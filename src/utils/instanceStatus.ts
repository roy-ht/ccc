import { InstanceStatus } from "../types";

export type StatusShape = "dot" | "ring" | "spinner" | "cross" | "bang";
export type StatusAnimation = "pulse" | "spin" | "blink" | "bounce";

export interface StatusDescriptor {
  shape: StatusShape;
  color: string;
  animation?: StatusAnimation;
}

export function statusDescriptor(status: InstanceStatus): StatusDescriptor {
  switch (status) {
    // busy はコメットスピナー（connecting と同形だが緑・高速回転・グロー。CSS 側で差別化）
    case "agent_busy":          return { shape: "spinner", color: "#4ade80", animation: "spin" };
    case "running":             return { shape: "dot",     color: "#60a5fa" };
    case "agent_idle":          return { shape: "ring",    color: "#facc15" };
    case "agent_waiting_input": return { shape: "bang",    color: "#fb923c", animation: "bounce" };
    case "connecting":          return { shape: "spinner", color: "#94a3b8", animation: "spin" };
    case "disconnected":        return { shape: "cross",   color: "#f87171" };
    case "terminated":          return { shape: "ring",    color: "#475569" };
  }
}

export function statusLabel(status: InstanceStatus): string {
  switch (status) {
    case "agent_busy":          return "作業中";
    case "agent_idle":          return "待機中";
    case "agent_waiting_input": return "指示待ち";
    case "running":             return "アクティブ";
    case "connecting":          return "接続中...";
    case "disconnected":        return "切断";
    case "terminated":          return "終了";
  }
}
