import { MainTab } from "../types";

interface Props {
  active: MainTab;
  onChange: (tab: MainTab) => void;
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
];

/// 主画面上部のタブバー（Agent / Terminal / Sessions / Memories / Explorer）。
///
/// forward 管理はここには置かない。インスタンスに紐づけると起動していないホストの
/// forward が見えず、ホストを跨いだポート衝突を管理できないため、設定画面の
/// 横断ビューへ移した。
export function MainTabs({ active, onChange }: Props) {
  return (
    <div className="main-tabs" role="tablist">
      {TABS.map((t) => (
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
