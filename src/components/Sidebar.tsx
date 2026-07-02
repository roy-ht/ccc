import { useState } from "react";
import {
  DndContext,
  DragEndEvent,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import {
  SortableContext,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { InstanceInfo, InstanceId, PromptOption } from "../types";
import { statusLabel } from "../utils/instanceStatus";
import { StatusIndicator } from "./StatusIndicator";
import { useStatusTransitions } from "../hooks/useStatusTransitions";
import { TypewriterText, useChangeSeq } from "./TypewriterText";

interface Props {
  instances: InstanceInfo[];
  activeId: InstanceId | null;
  onSelect: (id: InstanceId) => void;
  onClose: (id: InstanceId) => void;
  onReconnect: (id: InstanceId) => void;
  onRecreate: (id: InstanceId) => void;
  onNew: () => void;
  onOpenSettings: () => void;
  onReorder: (fromId: InstanceId, toId: InstanceId) => void;
  onPromptResponse: (id: InstanceId, key: string) => void;
  width: number;
  onStartResize: (e: React.MouseEvent) => void;
  onResetWidth: () => void;
}

export function Sidebar({
  instances,
  activeId,
  onSelect,
  onClose,
  onReconnect,
  onRecreate,
  onNew,
  onOpenSettings,
  onReorder,
  onPromptResponse,
  width,
  onStartResize,
  onResetWidth,
}: Props) {
  const [showHelp, setShowHelp] = useState(false);

  // 状態変化の未読マーカー
  const { unread } = useStatusTransitions(instances, activeId);

  // クリックとドラッグを区別するため 4px の移動でアクティブ化する
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } })
  );

  const handleDragEnd = (e: DragEndEvent) => {
    const { active, over } = e;
    if (!over || active.id === over.id) return;
    onReorder(active.id as InstanceId, over.id as InstanceId);
  };

  return (
    <aside className="sidebar" style={{ width, minWidth: width }}>
      <div className="sidebar-header">
        <span className="sidebar-title">Instances</span>
        <div className="sidebar-actions">
          <button className="btn-new" onClick={onNew} title="新規インスタンス (⌘T)">
            New +
          </button>
        </div>
      </div>

      <DndContext
        sensors={sensors}
        collisionDetection={closestCenter}
        onDragEnd={handleDragEnd}
      >
        <SortableContext
          items={instances.map((s) => s.id)}
          strategy={verticalListSortingStrategy}
        >
          <ul className="instance-list">
            {instances.length === 0 && (
              <li className="instance-empty">インスタンスなし</li>
            )}
            {instances.map((s) => (
              <SortableInstanceItem
                key={s.id}
                instance={s}
                isActive={s.id === activeId}
                unreadColor={unread.get(s.id)}
                onSelect={onSelect}
                onClose={onClose}
                onReconnect={onReconnect}
                onRecreate={onRecreate}
                onPromptResponse={onPromptResponse}
              />
            ))}
          </ul>
        </SortableContext>
      </DndContext>

      <div className="sidebar-footer">
        <button className="btn-settings" onClick={onOpenSettings} title="設定">⚙</button>
        <div className="help-wrap">
          <button
            className="btn-help"
            onClick={() => setShowHelp((v) => !v)}
            title="ショートカット一覧"
          >?</button>
          {showHelp && (
            <div className="help-popover" onClick={(e) => e.stopPropagation()}>
              <div className="help-popover-title">ショートカット</div>
              <ul className="help-popover-list">
                <li><kbd>⌘T</kbd> 新規インスタンス</li>
                <li><kbd>⌘[ / ⌘]</kbd> 前後に切替</li>
                <li><kbd>⌘1</kbd>〜<kbd>⌘9</kbd> N番目を選択</li>
              </ul>
              <button className="btn-cancel btn-sm" onClick={() => setShowHelp(false)}>閉じる</button>
            </div>
          )}
        </div>
      </div>

      <div
        className="sidebar-resize-handle"
        onMouseDown={onStartResize}
        onDoubleClick={onResetWidth}
        role="separator"
        aria-orientation="vertical"
        aria-label="サイドバー幅を調整（ダブルクリックでリセット）"
        title="ドラッグで幅調整 / ダブルクリックでリセット"
      />
    </aside>
  );
}

interface SortableInstanceItemProps {
  instance: InstanceInfo;
  isActive: boolean;
  /** 未読変化マーカーの表示色（undefined = 未読なし） */
  unreadColor?: string;
  onSelect: (id: InstanceId) => void;
  onClose: (id: InstanceId) => void;
  onReconnect: (id: InstanceId) => void;
  onRecreate: (id: InstanceId) => void;
  onPromptResponse: (id: InstanceId, key: string) => void;
}

function SortableInstanceItem({
  instance,
  isActive,
  unreadColor,
  onSelect,
  onClose,
  onReconnect,
  onRecreate,
  onPromptResponse,
}: SortableInstanceItemProps) {
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: instance.id });

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.4 : 1,
  };

  const cls = [
    "instance-item",
    isActive ? "active" : "",
    instance.status === "disconnected" ? "disconnected" : "",
    isDragging ? "dragging" : "",
    instance.status === "agent_waiting_input" ? "waiting-input" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <li
      ref={setNodeRef}
      style={style}
      className={cls}
      onClick={() => onSelect(instance.id)}
    >
      {unreadColor && (
        <span
          className="instance-unread-dot"
          style={{ "--unread-color": unreadColor } as React.CSSProperties}
          title="未読の状態変化"
        />
      )}
      <span
        className="instance-drag-handle"
        {...attributes}
        {...listeners}
        onClick={(e) => e.stopPropagation()}
        title="ドラッグして並び替え"
      >
        ≡
      </span>
      <div className="instance-content">
        <div className="instance-row">
          <StatusIndicator status={instance.status} />
          <span
            className={`instance-kind-chip kind-${instance.kind}`}
            title={instance.kind === "remote" ? "Remote (SSH)" : "Local"}
          >
            {instance.kind === "remote" ? "R" : "L"}
          </span>
          <span
            className={`instance-name kind-${instance.kind}`}
            title={instance.name}
          >
            {instance.name}
          </span>
          <button
            className="btn-close"
            title="インスタンスを閉じる"
            onClick={(e) => { e.stopPropagation(); onClose(instance.id); }}
          >
            ×
          </button>
        </div>

        {instance.session_title && (
          <div className="instance-session-title" title={instance.session_title}>
            {instance.session_title}
          </div>
        )}
        <div className="instance-status">{statusLabel(instance.status)}</div>

        <StatusMessage instance={instance} onPromptResponse={onPromptResponse} />

        {(instance.status === "disconnected" || instance.status === "terminated") && (
          <div className="instance-recovery-actions">
            {instance.status === "disconnected" && (
              <button
                className="btn-reconnect"
                onClick={(e) => { e.stopPropagation(); onReconnect(instance.id); }}
                title="同じ tmux セッションへ再接続を試みる"
              >
                再接続
              </button>
            )}
            <button
              className="btn-recreate"
              onClick={(e) => { e.stopPropagation(); onRecreate(instance.id); }}
              title="同じ設定で新しいインスタンスを作り直す（古い側は閉じる）"
            >
              再作成
            </button>
          </div>
        )}
      </div>
    </li>
  );
}

function StatusMessage({
  instance,
  onPromptResponse,
}: {
  instance: InstanceInfo;
  onPromptResponse: (id: InstanceId, key: string) => void;
}) {
  // メッセージ更新時もタイプライター演出（hook は早期 return より前に呼ぶ）
  const message = instance.status_message ?? "";
  const messageSeq = useChangeSeq(message);

  const status = instance.status;
  if (status === "disconnected" || status === "terminated") return null;

  // 指示待ち（permission）はボタン群、plan は1行表示
  if (status === "agent_waiting_input" && instance.pending_prompt) {
    if (instance.pending_prompt.kind === "permission") {
      return (
        <div className="instance-prompt">
          <div className="instance-message" title={instance.pending_prompt.description}>
            {instance.pending_prompt.description || "確認待ち"}
          </div>
          <div className="prompt-buttons">
            {instance.pending_prompt.options.map((opt: PromptOption) => (
              <button
                key={opt.key}
                className={`prompt-btn prompt-btn-${classifyOption(opt.label)}`}
                onClick={(e) => { e.stopPropagation(); onPromptResponse(instance.id, opt.key); }}
                title={opt.label}
              >
                {opt.label}
              </button>
            ))}
          </div>
        </div>
      );
    }
    if (instance.pending_prompt.kind === "plan") {
      return <div className="instance-message instance-message-prompt">plan 選択待ち</div>;
    }
  }

  if (instance.status_message) {
    return (
      <div className="instance-message" title={instance.status_message}>
        <TypewriterText text={instance.status_message} trigger={messageSeq} durationMs={320} />
      </div>
    );
  }

  return null;
}

/// ラベルから permission ボタンの色分けを推定する。
function classifyOption(label: string): "yes" | "no" | "neutral" {
  const lower = label.toLowerCase();
  if (lower.startsWith("no") || lower.includes("don't run") || lower.includes("keep planning")) {
    return "no";
  }
  if (lower.startsWith("yes")) {
    return "yes";
  }
  return "neutral";
}
