import React, { useState, useEffect, useRef } from "react";
import { useProfilerGridContext } from "../contexts/ProfilerGridContext";
import { DEFAULT_HEAT_COLOR, DEFAULT_HEAT_STYLE } from "../profilerConfig.ts";
import { COUNT_DIM_KEYS } from "../columnsConfig.ts";

interface NumberFormatConfig {
  group: string;
  decimal: string;
  decimals?: number;
}

interface NumberParts {
  sign: string;
  int: string;
  frac: string;
  decimal: string;
}

function formatNumberParts(value: number, { group, decimal, decimals }: NumberFormatConfig): NumberParts {
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const fixed = Number.isFinite(decimals) ? abs.toFixed(decimals) : String(abs);
  const [intPart, fracPart] = fixed.split(".");
  const grouped = group
    ? intPart.replace(/\B(?=(\d{3})+(?!\d))/g, group)
    : intPart;
  return {
    sign,
    int: grouped,
    frac: fracPart || "",
    decimal,
  };
}

interface FormattedNumberProps {
  value: number | string | null | undefined;
  format?: NumberFormatConfig | null;
  decimals?: number;
  className?: string;
}

function FormattedNumber({ value, format, decimals, className }: FormattedNumberProps) {
  if (value === null || value === undefined) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) {
    if (typeof value === "string" && value.trim() === "0") {
      return <span className={`${className} numeric-zero`}>-</span>;
    }
    return <span className={className}>{String(value)}</span>;
  }
  if (value === 0) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  const parts = formatNumberParts(value, {
    group: format?.group ?? ",",
    decimal: format?.decimal ?? ".",
    decimals,
  });
  return (
    <span className={className}>
      {parts.sign}
      {parts.int}
      {parts.frac ? (
        <span className="numeric-decimals">
          {parts.decimal}
          {parts.frac}
        </span>
      ) : null}
    </span>
  );
}

function isCompressed(row: Record<string, unknown>) {
  return Number.isFinite(row.chain_count as number) && (row.chain_count as number) > 0;
}

function isZeroDisplay(value: unknown): boolean {
  if (value === null || value === undefined) return false;
  if (typeof value === "number") return value === 0;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "0") return true;
  }
  return false;
}

function hasDisplayValue(value: unknown): boolean {
  if (value === null || value === undefined) return false;
  if (typeof value === "number" && value === 0) return false;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "-") return false;
    if (trimmed === "0") return false;
    if (trimmed.includes("/")) {
      const [left, right] = trimmed.split("/").map((part) => Number(part));
      if (Number.isFinite(left) && Number.isFinite(right)) {
        return !(left === 0 && right === 0);
      }
    }
  }
  return true;
}

function formatSideValue(value: number, format: NumberFormatConfig | null | undefined, decimals: number): string {
  if (!Number.isFinite(value)) return String(value);
  if (value === 0) return "-";
  const parts = formatNumberParts(value, {
    group: format?.group ?? ",",
    decimal: format?.decimal ?? ".",
    decimals,
  });
  return `${parts.sign}${parts.int}${parts.frac ? `${parts.decimal}${parts.frac}` : ""}`;
}

// Legacy HeatCell that receives all props directly (for backwards compatibility)
interface HeatCellProps {
  row: Record<string, unknown>;
  value: number | string | null | undefined;
  percent: number;
  format: { decimals?: number };
  numberFormat: NumberFormatConfig;
  heatStyle: string;
  heatColor: string;
}

export const HeatCell = React.memo(function HeatCell({ row, value, percent, format, numberFormat, heatStyle, heatColor }: HeatCellProps) {
  const pct = Number.isFinite(percent) ? Math.max(0, Math.min(100, percent)) : 0;
  const level = pct > 0 ? pct / 100 : 0;
  const alpha = level > 0 ? 0.04 + level * 0.2 : 0;
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const dimmed = isZeroDisplay(value);
  return (
    <div
      className={`heat-cell heat-style-${heatStyle || "fill"} heat-color-${
        heatColor || "red"
      }`}
      style={{
        "--heat-alpha": alpha,
        "--heat-level": level,
        "--heat-width": `${pct}%`,
      } as React.CSSProperties}
    >
      <span className="heat-fill" />
      <span className="heat-edge" />
      <span className="heat-meter" />
      <div className={`heat-text${dimmed ? " numeric-zero" : ""}`}>
        {showAggregate ? (
          <span className="aggregate-badge" title="Aggregated via chain compression">
            Σ
          </span>
        ) : null}
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format.decimals}
          className="numeric-value"
        />
      </div>
    </div>
  );
});

interface NumericCellProps {
  row: Record<string, unknown>;
  value: number | string | null | undefined;
  format: { decimals?: number };
  numberFormat: NumberFormatConfig;
}

export function NumericCell({ row, value, format, numberFormat }: NumericCellProps) {
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const isPair = typeof value === "string" && value.includes("/") && value.trim() !== "-";
  const dimmed = !isPair && isZeroDisplay(value);
  return (
    <div className={`numeric-cell${dimmed ? " numeric-zero" : ""}`}>
      {showAggregate ? (
        <span className="aggregate-badge" title="Aggregated via chain compression">
          Σ
        </span>
      ) : null}
      {isPair ? (
        <span className="numeric-text">
          {(() => {
            const [leftRaw, rightRaw] = value.split("/");
            const left = Number(leftRaw);
            const right = Number(rightRaw);
            const decimals = format?.decimals ?? 0;
            const leftFormatted = Number.isFinite(left)
              ? formatSideValue(left, numberFormat, decimals)
              : leftRaw;
            const rightFormatted = Number.isFinite(right)
              ? formatSideValue(right, numberFormat, decimals)
              : rightRaw;
            const leftZero = Number.isFinite(left) && left === 0;
            const rightZero = Number.isFinite(right) && right === 0;
            return (
              <>
                <span className={leftZero ? "numeric-zero-part" : undefined}>{leftFormatted}</span>
                <span className="numeric-separator">/</span>
                <span className={rightZero ? "numeric-zero-part" : undefined}>{rightFormatted}</span>
              </>
            );
          })()}
        </span>
      ) : (
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format?.decimals ?? 0}
          className="numeric-text"
        />
      )}
    </div>
  );
}

/**
 * Context-aware HeatCell that gets heat configuration from context.
 * This allows the column definition to be stable since all dynamic values
 * are fetched at render time from context.
 */
interface ContextCellProps {
  row: Record<string, any>;
  column: Record<string, any>;
}

export function ContextHeatCell({ row, column }: ContextCellProps) {
  const ctx = useProfilerGridContext();
  const { getHeatBounds, getColumnValue, heatColorByKey, heatStyleByKey, numberFormat, heatConfig } = ctx;
  
  const colDef = column._colDef; // Original column definition stored on the column
  const colKey = colDef.key;
  
  const raw = getColumnValue(colKey, row);
  const bounds = getHeatBounds(colKey);
  const max = bounds?.max ?? 0;
  const min = bounds?.min ?? 0;
  const enabled = heatConfig?.[colKey]?.enabled ?? false;
  const pct =
    enabled && raw != null && raw > 0 && max > min
      ? ((raw - min) / (max - min)) * 100
      : 0;
  const value = colDef.getter ? colDef.getter(row) : row[colDef.key] ?? "-";
  const heatStyle = heatStyleByKey?.[colKey] || DEFAULT_HEAT_STYLE;
  const heatColor = heatColorByKey?.[colKey] || DEFAULT_HEAT_COLOR;
  const format = colDef.format || {};
  
  const level = pct > 0 ? pct / 100 : 0;
  const alpha = level > 0 ? 0.04 + level * 0.2 : 0;
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const dimmed = isZeroDisplay(value);
  
  return (
    <div
      className={`heat-cell heat-style-${heatStyle} heat-color-${heatColor}`}
      style={{
        "--heat-alpha": alpha,
        "--heat-level": level,
        "--heat-width": `${pct}%`,
      } as React.CSSProperties}
    >
      <span className="heat-fill" />
      <span className="heat-edge" />
      <span className="heat-meter" />
      <div className={`heat-text${dimmed ? " numeric-zero" : ""}`}>
        {showAggregate ? (
          <span className="aggregate-badge" title="Aggregated via chain compression">
            Σ
          </span>
        ) : null}
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format.decimals}
          className="numeric-value"
        />
      </div>
    </div>
  );
}

/**
 * Context-aware NumericCell that gets number format from context.
 */
export function ContextNumericCell({ row, column }: ContextCellProps) {
  const ctx = useProfilerGridContext();
  const { numberFormat } = ctx;
  
  const colDef = column._colDef;
  const value = colDef.getter ? colDef.getter(row) : row[colDef.key] ?? "-";
  const format = colDef.format || {};
  const dimZero = COUNT_DIM_KEYS.has(colDef.key);
  
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const isPair = typeof value === "string" && value.includes("/") && value.trim() !== "-";
  const dimmed = !isPair && isZeroDisplay(value);
  
  return (
    <div className={`numeric-cell${dimmed ? " numeric-zero" : ""}`}>
      {showAggregate ? (
        <span className="aggregate-badge" title="Aggregated via chain compression">
          Σ
        </span>
      ) : null}
      {isPair ? (
        <span className="numeric-text">
          {(() => {
            const [leftRaw, rightRaw] = value.split("/");
            const left = Number(leftRaw);
            const right = Number(rightRaw);
            const decimals = format?.decimals ?? 0;
            const leftFormatted = Number.isFinite(left)
              ? formatSideValue(left, numberFormat, decimals)
              : leftRaw;
            const rightFormatted = Number.isFinite(right)
              ? formatSideValue(right, numberFormat, decimals)
              : rightRaw;
            const leftZero = Number.isFinite(left) && left === 0;
            const rightZero = Number.isFinite(right) && right === 0;
            return (
              <>
                <span className={leftZero ? "numeric-zero-part" : undefined}>{leftFormatted}</span>
                <span className="numeric-separator">/</span>
                <span className={rightZero ? "numeric-zero-part" : undefined}>{rightFormatted}</span>
              </>
            );
          })()}
        </span>
      ) : (
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format?.decimals ?? 0}
          className="numeric-text"
        />
      )}
    </div>
  );
}

// =============================================================================
// Context-Aware Header Cells
// =============================================================================

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
interface DeferredInputProps {
  value: string | number | null;
  onChange: (val: string) => void;
  [key: string]: unknown;
}

function DeferredInput({ value, onChange, ...props }: DeferredInputProps) {
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
      {...props}
      value={localValue}
      onChange={(e) => setLocalValue(e.target.value)}
      onBlur={commit}
      onKeyDown={(e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
          (e.target as HTMLInputElement).blur();
        }
      }}
    />
  );
}

/**
 * Context-aware HeatHeaderCell - fetches all configuration from ProfilerGridContext
 * instead of receiving props directly. This allows the column definition to remain stable.
 */
interface HeaderCellProps {
  column: Record<string, any>;
  cell: Record<string, any>;
}

export function ContextHeatHeaderCell({ column, cell }: HeaderCellProps) {
  const ctx = useProfilerGridContext();
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  
  const colDef = column._colDef;
  const colKey = colDef?.key;
  
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
  
  if (!colKey) {
    return <div className="column-header">{cell.text}</div>;
  }
  
  // Get configuration from context (reads fresh values on each render)
  const heatConfig = ctx.heatConfig;
  const config = heatConfig?.[colKey] || { enabled: false, min: null, max: null };
  const heatStyle = ctx.heatStyleByKey?.[colKey] || ctx.defaultHeatStyle || DEFAULT_HEAT_STYLE;
  const heatColor = ctx.heatColorByKey?.[colKey] || ctx.defaultHeatColor || DEFAULT_HEAT_COLOR;
  const heatColorOptions = ctx.heatColorOptions || [];
  
  return (
    <div className="column-header" ref={containerRef}>
      <span className="column-header-label">{cell.text}</span>
      <button
        type="button"
        className="column-header-btn"
        onClick={(event: React.MouseEvent) => {
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
                onChange={() => ctx.toggleHeat(colKey)}
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
                onChange={(val) => ctx.setHeatMin(colKey, val === "" ? null : Number(val))}
              />
            </div>
            <div className="column-popover-row">
              <label className="column-popover-label">Max</label>
              <DeferredInput
                type="number"
                className="column-popover-input"
                placeholder="auto"
                value={config.max}
                onChange={(val) => ctx.setHeatMax(colKey, val === "" ? null : Number(val))}
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
                onChange={(event) => ctx.setHeatStyleForKey(colKey, event.target.value)}
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
                {heatColorOptions.map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    className={`color-swatch color-swatch-${option.id} ${heatColor === option.id ? "color-swatch-selected" : ""}`}
                    onClick={() => ctx.setHeatColorForKey(colKey, option.id)}
                    aria-label={option.label}
                    title={option.label}
                  />
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Context-aware SpanHeaderCell - fetches configuration from ProfilerGridContext
 */
export function ContextSpanHeaderCell({ column, cell }: HeaderCellProps) {
  const ctx = useProfilerGridContext();
  const [isOpen, setIsOpen] = useState(false);
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
  
  // Get configuration from context
  const spanVizConfig = ctx.spanVizConfig || { enabled: true, style: "fill", metric: "wall_inc_total" };
  const metricOptions = ctx.metricOptions || [
    { id: "wall_inc_total", label: "Wall Time (Inclusive Total)" },
    { id: "wall_inc_avg", label: "Wall Time (Inclusive Avg)" },
    { id: "wall_self_total", label: "Wall Time (Self Total)" },
    { id: "wall_self_avg", label: "Wall Time (Self Avg)" },
    { id: "busy_inc_total", label: "Busy Time (Inclusive Total)" },
    { id: "busy_inc_avg", label: "Busy Time (Inclusive Avg)" },
    { id: "busy_self_total", label: "Busy Time (Self Total)" },
    { id: "busy_self_avg", label: "Busy Time (Self Avg)" },
    { id: "wait_inc_total", label: "Wait Time (Inclusive Total)" },
    { id: "wait_inc_avg", label: "Wait Time (Inclusive Avg)" },
    { id: "wait_self_total", label: "Wait Time (Self Total)" },
    { id: "wait_self_avg", label: "Wait Time (Self Avg)" },
    { id: "clarity_runtime_total", label: "Clarity Runtime" },
  ];
  
  return (
    <div className="column-header" ref={containerRef}>
      <span className="column-header-label">{cell?.text || column?.label || "Span"}</span>
      <button
        type="button"
        className="column-header-btn"
        onClick={(event) => {
          event.stopPropagation();
          setIsOpen(!isOpen);
        }}
        aria-label="Span visualization settings"
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
                onChange={() => ctx.setSpanVizConfig({ ...spanVizConfig, enabled: !spanVizConfig.enabled })}
              />
              <span>Visualization enabled</span>
            </label>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Metric</label>
              <select
                className="column-popover-select"
                value={spanVizConfig.metric || "wall_inc_total"}
                onChange={(event) => ctx.setSpanVizConfig({ ...spanVizConfig, metric: event.target.value })}
              >
                {metricOptions.map((option) => (
                  <option key={option.id} value={option.id}>
                    {option.label}
                  </option>
                ))}
              </select>
            </div>
          </div>

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Style</label>
              <select
                className="column-popover-select"
                value={spanVizConfig.style || "fill"}
                onChange={(event) => ctx.setSpanVizConfig({ ...spanVizConfig, style: event.target.value })}
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
                {(ctx.heatColorOptions || []).map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    className={`color-swatch color-swatch-${option.id} ${spanVizConfig.color === option.id ? "color-swatch-selected" : ""}`}
                    onClick={() => ctx.setSpanVizConfig({ ...spanVizConfig, color: option.id })}
                    aria-label={option.label}
                    title={option.label}
                  />
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// =============================================================================
// Context-Aware Cells for Transactions Grid
// =============================================================================

// Import the transactions context dynamically to avoid circular deps
import { useTransactionsGridContext } from "../contexts/TransactionsGridContext";

/**
 * Context-aware HeatCell for Transactions grid.
 * Fetches heat configuration from TransactionsGridContext.
 */
export function TxContextHeatCell({ row, column }: ContextCellProps) {
  const ctx = useTransactionsGridContext();
  const { heatConfig, heatMaxes, heatStyleByKey, heatColorByKey, numberFormat } = ctx;
  
  const colKey = column._colKey;
  const decimals = column._decimals ?? 0;
  const value = row[colKey];
  
  const config = heatConfig?.[colKey] || { enabled: false, min: null, max: null };
  const heatStyle = heatStyleByKey?.[colKey] || DEFAULT_HEAT_STYLE;
  const heatColor = heatColorByKey?.[colKey] || DEFAULT_HEAT_COLOR;
  
  if (!config.enabled) {
    // No heat, just show the value
    const formatted = formatNumberSimple(value, numberFormat, decimals);
    const dimmed = value === 0 || value === null || value === undefined;
    return (
      <span className={`numeric-cell${dimmed ? " numeric-zero" : ""}`}>
        {formatted}
      </span>
    );
  }
  
  const minVal = config.min ?? 0;
  const maxVal = config.max ?? heatMaxes?.[colKey] ?? 1;
  const range = maxVal - minVal || 1;
  const pct = typeof value === "number" ? Math.max(0, Math.min(100, ((value - minVal) / range) * 100)) : 0;
  const level = pct > 0 ? pct / 100 : 0;
  const alpha = level > 0 ? 0.04 + level * 0.2 : 0;
  const dimmed = value === 0 || value === null || value === undefined;
  const formatted = formatNumberSimple(value, numberFormat, decimals);
  
  return (
    <div
      className={`heat-cell heat-style-${heatStyle} heat-color-${heatColor}`}
      style={{
        "--heat-alpha": alpha,
        "--heat-level": level,
        "--heat-width": `${pct}%`,
      } as React.CSSProperties}
    >
      <span className="heat-fill" />
      <span className="heat-edge" />
      <span className="heat-meter" />
      <div className={`heat-text${dimmed ? " numeric-zero" : ""}`}>
        {formatted}
      </div>
    </div>
  );
}

/**
 * Context-aware HeatHeaderCell for Transactions grid.
 */
export function TxContextHeatHeaderCell({ column, cell }: HeaderCellProps) {
  const ctx = useTransactionsGridContext();
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  
  const colKey = column._colKey;
  const isWallColumn = column._isWallColumn;
  
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
  
  if (!colKey) {
    return <div className="column-header">{cell.text}</div>;
  }
  
  const heatConfig = ctx.heatConfig;
  const config = heatConfig?.[colKey] || { enabled: false, min: null, max: null };
  const heatStyle = ctx.heatStyleByKey?.[colKey] || DEFAULT_HEAT_STYLE;
  const heatColor = ctx.heatColorByKey?.[colKey] || DEFAULT_HEAT_COLOR;
  const heatColorOptions = ctx.heatColorOptions || [];
  
  return (
    <div className="column-header" ref={containerRef}>
      <span className="column-header-label">{cell.text}</span>
      <button
        type="button"
        className="column-header-btn"
        onClick={(event: React.MouseEvent) => {
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
                onChange={() => ctx.toggleHeat(colKey)}
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
                onChange={(val) => ctx.setHeatMin(colKey, val === "" ? null : Number(val))}
              />
            </div>
            <div className="column-popover-row">
              <label className="column-popover-label">Max</label>
              <DeferredInput
                type="number"
                className="column-popover-input"
                placeholder="auto"
                value={config.max}
                onChange={(val) => ctx.setHeatMax(colKey, val === "" ? null : Number(val))}
              />
            </div>
          </div>

          {isWallColumn && (
            <>
              <div className="column-popover-divider" />
              <div className="column-popover-section">
                <div className="column-popover-row">
                  <label className="column-popover-label">Min (ms)</label>
                  <DeferredInput
                    type="number"
                    className="column-popover-input"
                    placeholder="0"
                    value={ctx.minWallFilterMs ?? null}
                    onChange={(val) => ctx.setMinWallFilterMs(val === "" ? null : Number(val))}
                  />
                </div>
              </div>
            </>
          )}

          <div className="column-popover-divider" />

          <div className="column-popover-section">
            <div className="column-popover-row">
              <label className="column-popover-label">Style</label>
              <select
                className="column-popover-select"
                value={heatStyle}
                onChange={(event) => ctx.setHeatStyleForKey(colKey, event.target.value)}
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
                {heatColorOptions.map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    className={`color-swatch color-swatch-${option.id} ${heatColor === option.id ? "color-swatch-selected" : ""}`}
                    onClick={() => ctx.setHeatColorForKey(colKey, option.id)}
                    aria-label={option.label}
                    title={option.label}
                  />
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Context-aware ActionCell for Transactions grid.
 */
export function TxContextActionCell({ row }: { row: Record<string, any> }) {
  const ctx = useTransactionsGridContext();
  return (
    <button
      className="transactions-view-btn"
      onClick={(e) => {
        e.stopPropagation();
        ctx.onViewTrace(row);
      }}
      title="View trace"
    >
      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
        <polyline points="15 3 21 3 21 9" />
        <line x1="10" y1="14" x2="21" y2="3" />
      </svg>
    </button>
  );
}

// Simple number formatter for transactions grid
function formatNumberSimple(value: number | string | null | undefined, format: NumberFormatConfig | null | undefined, decimals = 0): string {
  if (value === null || value === undefined) return "-";
  if (typeof value !== "number" || !Number.isFinite(value)) return String(value);
  if (value === 0) return "-";
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const fixed = Number.isFinite(decimals) ? abs.toFixed(decimals) : String(abs);
  const [intPart, fracPart] = fixed.split(".");
  const grouped = format?.group
    ? intPart.replace(/\B(?=(\d{3})+(?!\d))/g, format.group)
    : intPart;
  return `${sign}${grouped}${fracPart ? `${format?.decimal || "."}${fracPart}` : ""}`;
}
