import { MainTab } from "../types";

interface Props {
  active: MainTab;
  onChange: (tab: MainTab) => void;
  /** Forwards タブを表示するか（リモートインスタンスのみ true） */
  showForwards: boolean;
}

const TABS: { id: MainTab; label: string }[] = [
  // id="terminal" は内部キー。表示はターミナル=エージェント操作面なので "Agent"。
  { id: "terminal", label: "Agent" },
  // id="shell" は agent と同じ tmux session の session-group メンバーへ attach する
  // 補助 PTY。表示名は素直に "Terminal"。
  { id: "shell", label: "Terminal" },
  { id: "sessions", label: "Sessions" },
  { id: "memories", label: "Memories" },
  { id: "explorer", label: "Explorer" },
  { id: "forwards", label: "Forwards" },
];

/** 主画面上部のタブバー（Agent / Sessions / Memories / Explorer / Forwards）。 */
export function MainTabs({ active, onChange, showForwards }: Props) {
  const tabs = showForwards ? TABS : TABS.filter((t) => t.id !== "forwards");
  return (
    <div className="main-tabs" role="tablist">
      {tabs.map((t) => (
        <button
          key={t.id}
          role="tab"
          aria-selected={active === t.id}
          className={`main-tab ${active === t.id ? "active" : ""}`}
          onClick={() => onChange(t.id)}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}
