import React, { useState, useEffect, useRef } from "react";
import type { HeatConfigEntry, HeatColorOption } from "../contexts/ProfilerGridContext";

// Context kept for backwards compatibility but each cell now uses local state
export const HeatHeaderMenuContext = React.createContext<{ openId: string | null; setOpenId: (id: string | null) => void }>({
  openId: null,
  setOpenId: () => {},
});

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

// Local input that commits on blur or Enter
function DeferredInput({ value, onChange, ...props }: { value: string | number | null; onChange: (v: string) => void; [key: string]: unknown }) {
  const [localValue, setLocalValue] = useState(value ?? "");
  
  useEffect(() => {
    setLocalValue(value ?? "");
  }, [value]);
  
  const commit = () => {
    if (localValue !== (value ?? "")) {
      onChange(String(localValue));
    }
  };
  
  return (
    <input
      {...(props as React.InputHTMLAttributes<HTMLInputElement>)}
      value={localValue}
      onChange={(e) => setLocalValue(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
          (e.target as HTMLInputElement).blur();
        }
      }}
    />
  );
}

interface HeatHeaderCellProps {
  column: { id?: string; heatKey?: string; [key: string]: unknown };
  cell: { text: string };
  heatConfig: Record<string, HeatConfigEntry>;
  onToggle: (colKey: string) => void;
  onMinChange: (colKey: string, val: string) => void;
  onMaxChange: (colKey: string, val: string) => void;
  heatStyle: string;
  setHeatStyle: (colKey: string, style: string) => void;
  heatColor: string;
  setHeatColor: (colKey: string, color: string) => void;
  heatColorOptions: HeatColorOption[];
  minWallFilterMs?: number | null;
  setMinWallFilterMs?: (val: string) => void;
}

export default function HeatHeaderCell({
  column,
  cell,
  heatConfig,
  onToggle,
  onMinChange,
  onMaxChange,
  heatStyle,
  setHeatStyle,
  heatColor,
  setHeatColor,
  heatColorOptions,
  minWallFilterMs,
  setMinWallFilterMs,
}: HeatHeaderCellProps) {
  // Use local state for menu open/close
  const [isOpen, setIsOpen] = useState(false);
  const heatKey = column.heatKey as string | undefined;
  const containerRef = useRef<HTMLDivElement>(null);

  // Close menu on outside click
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
  if (!heatKey) {
    return <div className="column-header">{cell.text}</div>;
  }
  const config = heatConfig[heatKey] || { enabled: false, min: null, max: null };
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
                checked={config.enabled}
                onChange={() => onToggle(heatKey)}
              />
              <span>Heatmap enabled</span>
            </label>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Min</label>
              <DeferredInput
                type="number"
                className="column-popover-input"
                placeholder="auto"
                value={config.min}
                onChange={(val: string) => onMinChange(heatKey!, val)}
              />
            </div>
            <div className="column-popover-row">
              <label className="column-popover-label">Max</label>
              <DeferredInput
                type="number"
                className="column-popover-input"
                placeholder="auto"
                value={config.max}
                onChange={(val: string) => onMaxChange(heatKey!, val)}
              />
            </div>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Style</label>
              <select
                className="column-popover-select"
                value={heatStyle}
                onChange={(event) => setHeatStyle(heatKey, event.target.value)}
              >
                <option value="fill">Fill</option>
                <option value="edge">Edge</option>
                <option value="meter">Meter</option>
              </select>
            </div>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row-start">
              <label className="column-popover-label">Color</label>
              <div className="color-swatch-grid">
                {heatColorOptions.map((option: HeatColorOption) => (
                  <button
                    key={option.id}
                    type="button"
                    className={`color-swatch color-swatch-${option.id} ${heatColor === option.id ? "color-swatch-selected" : ""}`}
                    onClick={() => setHeatColor(heatKey, option.id)}
                    aria-label={option.label}
                    title={option.label}
                  />
                ))}
              </div>
            </div>
          </div>

          {((column.id === "wall_inc_total" || column.heatKey === "wallTotalUs") || (minWallFilterMs !== undefined && setMinWallFilterMs)) && (
            <>
              <div className="column-popover-divider" />
              <div className="column-popover-section">
                <div className="column-popover-row">
                  <label className="column-popover-label">Min (ms)</label>
                  <DeferredInput
                    type="number"
                    className="column-popover-input"
                    placeholder="off"
                    value={minWallFilterMs ?? ""}
                    onChange={(val: string) => setMinWallFilterMs?.(val)}
                  />
                </div>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
