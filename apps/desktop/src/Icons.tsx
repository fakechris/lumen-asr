/** Lumen design-system icons — 24×24, stroke 1.8, currentColor. */

import type { ReactNode } from "react";

export type IconName =
  | "mic"
  | "history"
  | "dictionary"
  | "learn"
  | "settings"
  | "overview"
  | "play"
  | "stop"
  | "copy"
  | "copy-check"
  | "refresh"
  | "delete"
  | "add"
  | "search"
  | "shield"
  | "sparkle-ai"
  | "hotkey"
  | "insert"
  | "translate"
  | "wave"
  | "clipboard"
  | "sun"
  | "moon";

const PATHS: Record<IconName, ReactNode> = {
  mic: (
    <>
      <rect x="9" y="2.5" width="6" height="11" rx="3" />
      <path d="M5 11a7 7 0 0 0 14 0" />
      <path d="M12 18v3" />
      <path d="M8.5 21h7" />
    </>
  ),
  history: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.5V12l3.2 2" />
    </>
  ),
  dictionary: (
    <>
      <path d="M12 6.5C10.4 5.2 8.2 4.6 5.5 4.6 4.9 4.6 4.5 5 4.5 5.6V17c0 .6.4 1 1 1 2.6 0 4.6.5 6.5 1.8" />
      <path d="M12 6.5c1.6-1.3 3.8-1.9 6.5-1.9.6 0 1 .4 1 1V17c0 .6-.4 1-1 1-2.6 0-4.6.5-6.5 1.8" />
      <path d="M12 6.5v13" />
    </>
  ),
  learn: (
    <>
      <path d="M12 3.5l1.7 4.3 4.3 1.7-4.3 1.7L12 15.5l-1.7-4.3L6 9.5l4.3-1.7z" />
      <path d="M18.3 14.6l.7 1.9 1.9.7-1.9.7-.7 1.9-.7-1.9-1.9-.7 1.9-.7z" />
    </>
  ),
  settings: (
    <>
      <path d="M4 8h11" />
      <circle cx="18" cy="8" r="2.4" />
      <path d="M20 16H9" />
      <circle cx="6" cy="16" r="2.4" />
    </>
  ),
  overview: (
    <>
      <rect x="3.5" y="3.5" width="7" height="7" rx="1.6" />
      <rect x="13.5" y="3.5" width="7" height="7" rx="1.6" />
      <rect x="3.5" y="13.5" width="7" height="7" rx="1.6" />
      <rect x="13.5" y="13.5" width="7" height="7" rx="1.6" />
    </>
  ),
  play: <path d="M8 5.5v13l11-6.5z" />,
  stop: <rect x="7.5" y="7.5" width="9" height="9" rx="2" />,
  copy: (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2.4" />
      <path d="M5 15H4.5A1.5 1.5 0 0 1 3 13.5V4.5A1.5 1.5 0 0 1 4.5 3h9A1.5 1.5 0 0 1 15 4.5V5" />
    </>
  ),
  "copy-check": (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2.4" />
      <path d="M5 15H4.5A1.5 1.5 0 0 1 3 13.5V4.5A1.5 1.5 0 0 1 4.5 3h9A1.5 1.5 0 0 1 15 4.5V5" />
      <path d="M11.5 14.3l1.9 1.9 3.6-3.8" />
    </>
  ),
  refresh: (
    <>
      <path d="M4.5 9a7.5 7.5 0 0 1 13-3.4L20 8" />
      <path d="M20 3.5V8h-4.5" />
      <path d="M19.5 15a7.5 7.5 0 0 1-13 3.4L4 16" />
      <path d="M4 20.5V16h4.5" />
    </>
  ),
  delete: (
    <>
      <path d="M4.5 7h15" />
      <path d="M9.5 7V5.5A1.5 1.5 0 0 1 11 4h2a1.5 1.5 0 0 1 1.5 1.5V7" />
      <path d="M6.5 7l.8 11a1.5 1.5 0 0 0 1.5 1.4h6.4a1.5 1.5 0 0 0 1.5-1.4L18.5 7" />
    </>
  ),
  add: (
    <>
      <path d="M12 5v14" />
      <path d="M5 12h14" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="M20 20l-4.2-4.2" />
    </>
  ),
  shield: (
    <>
      <path d="M12 3.2 19 6v5.5c0 4.3-3 7-7 8.8-4-1.8-7-4.5-7-8.8V6z" />
      <path d="M9 12l2.2 2.2L15.5 10" />
    </>
  ),
  "sparkle-ai": <path d="M12 4l1.5 4L18 9.5 13.5 11 12 15l-1.5-4L6 9.5 10.5 8z" />,
  hotkey: (
    <>
      <rect x="3" y="6" width="18" height="12" rx="2.4" />
      <path d="M7 10h.01M11 10h.01M15 10h.01M7 13.5h10" />
    </>
  ),
  insert: (
    <>
      <path d="M12 3v9" />
      <path d="M8.5 8.5 12 12l3.5-3.5" />
      <path d="M5 13v4.5A1.5 1.5 0 0 0 6.5 19h11a1.5 1.5 0 0 0 1.5-1.5V13" />
    </>
  ),
  translate: (
    <>
      <path d="M4 6h7" />
      <path d="M7.5 6V4.5" />
      <path d="M9.6 6c0 4-2.3 7.2-5.1 8.6" />
      <path d="M6 10.6c1.2 2 3 3.3 5 4" />
      <path d="M12.8 19.5l3.3-8.2 3.3 8.2" />
      <path d="M13.9 16.7h4.6" />
    </>
  ),
  wave: (
    <path
      d="M4 12h1M8 8v8M12 5v14M16 8v8M20 12h-1"
      strokeWidth="1.9"
    />
  ),
  clipboard: (
    <>
      <rect x="9" y="2.9" width="6" height="3.3" rx="1.2" />
      <rect x="5" y="4.5" width="14" height="16" rx="2.6" />
      <path d="M8.5 12h7M8.5 15.5h5" />
    </>
  ),
  sun: (
    <>
      <circle cx="12" cy="12" r="3.5" />
      <path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4" />
    </>
  ),
  moon: <path d="M20 15.2A8.6 8.6 0 0 1 8.8 4a8.7 8.7 0 1 0 11.2 11.2Z" />,
};

export function Icon({
  name,
  size = 18,
  className,
  label,
}: {
  name: IconName;
  size?: number;
  className?: string;
  /** Accessible name; omit for decorative icons. */
  label?: string;
}) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className ? `icon ${className}` : "icon"}
      aria-hidden={label ? undefined : true}
      role={label ? "img" : undefined}
      aria-label={label}
    >
      {PATHS[name]}
    </svg>
  );
}
