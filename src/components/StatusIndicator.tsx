import { InstanceStatus } from "../types";
import {
  StatusDescriptor,
  StatusShape,
  statusDescriptor,
  statusLabel,
} from "../utils/instanceStatus";

interface Props {
  status: InstanceStatus;
}

// 遷移の瞬間の演出はアイコンではなく行の縁で行う（Sidebar の .instance-beam）。
// アイコン自体の変形（ポップ/リップル）はチカチカするため廃止した。
export function StatusIndicator({ status }: Props) {
  const desc = statusDescriptor(status);
  const label = statusLabel(status);
  const className = [
    "status-indicator",
    `status-${status}`,
    `status-shape-${desc.shape}`,
    desc.animation ? `status-animate-${desc.animation}` : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <span
      className={className}
      style={{ "--status-color": desc.color } as React.CSSProperties}
      role="img"
      aria-label={label}
      title={label}
    >
      <ShapeSvg desc={desc} />
    </span>
  );
}

function ShapeSvg({ desc }: { desc: StatusDescriptor }) {
  const { shape, color } = desc;
  const props = { width: 14, height: 14, viewBox: "0 0 14 14" };
  switch (shape) {
    case "dot":
      return (
        <svg {...props}>
          <circle cx="7" cy="7" r="4" fill={color} />
        </svg>
      );
    case "ring":
      return (
        <svg {...props}>
          <circle
            cx="7"
            cy="7"
            r="4.5"
            fill="none"
            stroke={color}
            strokeWidth="1.5"
          />
        </svg>
      );
    case "spinner":
      return (
        <svg {...props}>
          <circle
            cx="7"
            cy="7"
            r="4.5"
            fill="none"
            stroke={color}
            strokeOpacity="0.25"
            strokeWidth="1.5"
          />
          <path
            d="M 7 2.5 A 4.5 4.5 0 0 1 11.5 7"
            fill="none"
            stroke={color}
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </svg>
      );
    case "cross":
      return (
        <svg {...props}>
          <path
            d="M3.5 3.5 L10.5 10.5 M10.5 3.5 L3.5 10.5"
            stroke={color}
            strokeWidth="2"
            strokeLinecap="round"
          />
        </svg>
      );
    case "bang":
      return (
        <svg {...props}>
          <path d="M7 1.5 L12.5 11.5 L1.5 11.5 Z" fill={color} />
          <rect x="6.4" y="5" width="1.2" height="3.6" rx="0.4" fill="#1f1f1f" />
          <rect x="6.4" y="9.5" width="1.2" height="1.2" rx="0.4" fill="#1f1f1f" />
        </svg>
      );
  }
}

export type { StatusShape };
