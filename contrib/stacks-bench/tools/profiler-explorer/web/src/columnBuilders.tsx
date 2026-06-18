import React from "react";
import HeatHeaderCell from "./components/HeatHeaderCell";
import SpanHeaderCell from "./components/SpanHeaderCell";
import type { HeatConfigEntry, HeatColorOption, SpanVizConfig } from "./contexts/ProfilerGridContext";

interface HeatHeaderBuilderConfig {
  heatConfig: Record<string, HeatConfigEntry>;
  toggleHeat: (colKey: string) => void;
  setHeatMin: (colKey: string, val: string) => void;
  setHeatMax: (colKey: string, val: string) => void;
  heatStyleByKey: Record<string, string>;
  setHeatStyleForKey: (colKey: string, style: string) => void;
  heatColorByKey: Record<string, string>;
  setHeatColorForKey: (colKey: string, color: string) => void;
  heatColorOptions: HeatColorOption[];
  minWallFilterMs: number | null;
  setMinWallFilterMs: (val: string) => void;
  defaultHeatStyle: string;
  defaultHeatColor: string;
}

interface ColumnDef {
  key?: string;
  id?: string;
  heatKey?: string;
  label?: string;
  headerLabel?: string;
  level3?: string;
  group?: string;
  groupStart?: boolean;
  groupSpan?: number;
  cell?: React.FC<Record<string, unknown>>;
  [key: string]: unknown;
}

/**
 * Creates a heat header builder function.
 */
export function createHeatHeaderBuilder({
  heatConfig,
  toggleHeat,
  setHeatMin,
  setHeatMax,
  heatStyleByKey,
  setHeatStyleForKey,
  heatColorByKey,
  setHeatColorForKey,
  heatColorOptions,
  minWallFilterMs,
  setMinWallFilterMs,
  defaultHeatStyle,
  defaultHeatColor,
}: HeatHeaderBuilderConfig) {
  return (col: ColumnDef) => ({
    text: col.level3 || col.headerLabel || col.label,
    cell: (props: any) => (
      <HeatHeaderCell
        {...props}
        heatConfig={heatConfig}
        onToggle={toggleHeat}
        onMinChange={setHeatMin}
        onMaxChange={setHeatMax}
        heatStyle={heatStyleByKey[col.heatKey!] || defaultHeatStyle}
        setHeatStyle={setHeatStyleForKey}
        heatColor={heatColorByKey[col.heatKey!] || defaultHeatColor}
        setHeatColor={setHeatColorForKey}
        heatColorOptions={heatColorOptions}
        minWallFilterMs={minWallFilterMs}
        setMinWallFilterMs={setMinWallFilterMs}
      />
    ),
  });
}

/**
 * Creates a span header builder function.
 * 
 * NOTE: The isOpen/setIsOpen props are intentionally NOT included here.
 * They are managed via SpanHeaderMenuContext to avoid recreating column
 * definitions every time the menu is opened/closed.
 */
export function createSpanHeaderBuilder({
  spanVizConfig,
  setSpanVizConfig,
}: { spanVizConfig: SpanVizConfig; setSpanVizConfig: (updater: (prev: SpanVizConfig) => SpanVizConfig) => void }) {
  return (col: ColumnDef) => ({
    text: col.label,
    cell: (props: any) => (
      <SpanHeaderCell
        {...props}
        spanVizConfig={spanVizConfig}
        setSpanVizConfig={setSpanVizConfig}
      />
    ),
  });
}

export function createGroupHeaderBuilder(buildHeatHeader: (col: ColumnDef) => Record<string, unknown>) {
  return (col: ColumnDef) => [
    col.groupStart
      ? { text: col.group, colspan: col.groupSpan, css: "grid-group-header" }
      : { text: "", _hidden: true },
    col.heatKey ? buildHeatHeader(col) : { text: col.headerLabel ?? col.label },
  ];
}
