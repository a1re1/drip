// Inline Lucide glyphs (stroke, currentColor) and the capsule icon button
// built on them, so the UI ships no icon font or asset directory.
import type { CSSProperties, ReactNode } from "react";

export type IconName =
  | "plus"
  | "pause"
  | "play"
  | "list"
  | "chevron-right"
  | "chevron-down"
  | "send"
  | "panel-left"
  | "x"
  | "clock"
  | "search"
  | "check"
  | "circle-alert";

const PATHS: Record<IconName, ReactNode> = {
  plus: (
    <>
      <path d="M5 12h14" />
      <path d="M12 5v14" />
    </>
  ),
  pause: (
    <>
      <rect x="14" y="3" width="5" height="18" rx="1" />
      <rect x="5" y="3" width="5" height="18" rx="1" />
    </>
  ),
  play: <path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" />,
  list: (
    <>
      <path d="M3 5h.01" />
      <path d="M3 12h.01" />
      <path d="M3 19h.01" />
      <path d="M8 5h13" />
      <path d="M8 12h13" />
      <path d="M8 19h13" />
    </>
  ),
  "chevron-right": <path d="m9 18 6-6-6-6" />,
  "chevron-down": <path d="m6 9 6 6 6-6" />,
  send: (
    <>
      <path d="M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11z" />
      <path d="m21.854 2.147-10.94 10.939" />
    </>
  ),
  "panel-left": (
    <>
      <rect width="18" height="18" x="3" y="3" rx="2" />
      <path d="M9 3v18" />
    </>
  ),
  x: (
    <>
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="10" />
      <path d="M12 6v6l4 2" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="8" />
      <path d="m21 21-4.3-4.3" />
    </>
  ),
  check: <path d="M20 6 9 17l-5-5" />,
  "circle-alert": (
    <>
      <circle cx="12" cy="12" r="10" />
      <path d="M12 8v4" />
      <path d="M12 16h.01" />
    </>
  ),
};

export function Icon({ name, size = 16, style }: { name: IconName; size?: number; style?: CSSProperties }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      style={{ flex: "none", ...style }}
    >
      {PATHS[name]}
    </svg>
  );
}

export type ButtonVariant = "primary" | "glass" | "plain" | "muted" | "destructive";

interface IconButtonProps {
  icon: IconName;
  label: string;
  variant?: ButtonVariant;
  size?: "s" | "m" | "l";
  pressed?: boolean;
  disabled?: boolean;
  onClick?: () => void;
  style?: CSSProperties;
  type?: "button" | "submit";
}

export function IconButton({ icon, label, variant = "glass", size = "m", pressed, disabled, onClick, style, type = "button" }: IconButtonProps) {
  const iconSize = size === "l" ? 16 : size === "s" ? 12 : 14;
  return (
    <button
      type={type}
      className={`vt-btn vt-iconbtn vt-btn--${variant}${size === "m" ? "" : ` vt-btn--${size}`}`}
      aria-label={label}
      title={label}
      aria-pressed={pressed}
      disabled={disabled}
      onClick={onClick}
      style={style}
    >
      <Icon name={icon} size={iconSize} />
    </button>
  );
}

export function Tag({ children, color }: { children: ReactNode; color?: string }) {
  return (
    <span className="vt-tag" style={color ? ({ "--vt-tag-fg": color } as CSSProperties) : undefined}>
      {children}
    </span>
  );
}
