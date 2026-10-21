import React, { useState, useRef, useEffect } from "react";
import type { SpanVizConfig } from "../contexts/ProfilerGridContext";

// Settings cog icon (Heroicons style)
function SettingsIcon() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M10 13a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z" />
      <path d="M16.4 12.7a1.2 1.2 0 0 0 .2 1.3l.1.1a1.5 1.5 0 0 1-1 2.6 1.5 1.5 0 0 1-1.1-.5l-.1-.1a1.2 1.2 0 0 0-1.3-.2 1.2 1.2 0 0 0-.7 1.1v.2a1.5 1.5 0 1 1-3 0v-.1a1.2 1.2 0 0 0-.8-1.1 1.2 1.2 0 0 0-1.3.2l-.1.1a1.5 1.5 0 1 1-2.1-2.1l.1-.1a1.2 1.2 0 0 0 .2-1.3 1.2 1.2 0 0 0-1.1-.7h-.2a1.5 1.5 0 1 1 0-3h.1a1.2 1.2 0 0 0 1.1-.8 1.2 1.2 0 0 0-.2-1.3l-.1-.1a1.5 1.5 0 1 1 2.1-2.1l.1.1a1.2 1.2 0 0 0 1.3.2h.1a1.2 1.2 0 0 0 .7-1.1v-.2a1.5 1.5 0 0 1 3 0v.1a1.2 1.2 0 0 0 .7 1.1 1.2 1.2 0 0 0 1.3-.2l.1-.1a1.5 1.5 0 1 1 2.1 2.1l-.1.1a1.2 1.2 0 0 0-.2 1.3v.1a1.2 1.2 0 0 0 1.1.7h.2a1.5 1.5 0 0 1 0 3h-.1a1.2 1.2 0 0 0-1.1.7Z" />
    </svg>
  );
}

const SPAN_VIZ_METRICS = [
  { key: "wallTotalUs", label: "Wall Total" },
  { key: "selfWallUs", label: "Wall Self" },
  { key: "busyTotalUs", label: "Busy Total" },
  { key: "selfBusyUs", label: "Busy Self" },
  { key: "waitTotalUs", label: "Wait Total" },
  { key: "selfWaitUs", label: "Wait Self" },
  { key: "clarityRuntime", label: "Clarity Runtime" },
];

interface SpanHeaderCellProps {
  cell: { text: string };
  spanVizConfig: SpanVizConfig;
  setSpanVizConfig: (updater: (prev: SpanVizConfig) => SpanVizConfig) => void;
}

export default function SpanHeaderCell({ cell, spanVizConfig, setSpanVizConfig }: SpanHeaderCellProps) {
  // Use local state for the menu open/close - this avoids recreating columns
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  // Close menu when clicking outside
  useEffect(() => {
    if (!isOpen) return;
    const handleOutsideClick = (event: MouseEvent) => {
      if (!containerRef.current) return;
      if (!containerRef.current.contains(event.target as Node)) {
        setIsOpen(false);
      }
    };
    document.addEventListener("mousedown", handleOutsideClick);
    return () => {
      document.removeEventListener("mousedown", handleOutsideClick);
    };
  }, [isOpen]);

  return (
    <div className="column-header" ref={containerRef}>
      <span className="column-header-label">{cell.text}</span>
      <button
        type="button"
        className="column-header-btn"
        onClick={(event) => {
          event.stopPropagation();
          setIsOpen(!isOpen);
        }}
        aria-label="Column settings"
        aria-expanded={isOpen}
      >
        <SettingsIcon />
      </button>
      {isOpen && (
        <div
          className="column-popover"
          onClick={(event) => event.stopPropagation()}
        >
          <div className="column-popover-section">
            <label className="column-popover-toggle">
              <input
                type="checkbox"
                checked={spanVizConfig.enabled}
                onChange={() =>
                  setSpanVizConfig((prev: SpanVizConfig) => ({ ...prev, enabled: !prev.enabled }))
                }
              />
              <span>Show visualization</span>
            </label>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Style</label>
              <select
                className="column-popover-select"
                value={spanVizConfig.style}
                onChange={(event) =>
                  setSpanVizConfig((prev: SpanVizConfig) => ({ ...prev, style: event.target.value }))
                }
              >
                <option value="fill">Fill</option>
                <option value="edge">Edge</option>
                <option value="meter">Meter</option>
              </select>
            </div>
            <div className="column-popover-row">
              <label className="column-popover-label">Metric</label>
              <select
                className="column-popover-select"
                value={spanVizConfig.metric}
                onChange={(event) =>
                  setSpanVizConfig((prev: SpanVizConfig) => ({ ...prev, metric: event.target.value }))
                }
              >
                {SPAN_VIZ_METRICS.map((m) => (
                  <option key={m.key} value={m.key}>
                    {m.label}
                  </option>
                ))}
              </select>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
