import React, { createContext, useContext, useRef } from "react";

/**
 * Context for profiler grid callbacks and configuration.
 * 
 * This context provides stable references to callbacks and config used by grid cell components.
 * By using refs internally, we ensure that:
 * 1. The context value remains stable (doesn't trigger re-renders of column definitions)
 * 2. Cell components always call the latest version of callbacks
 * 3. The visibleColumns array doesn't need to be recreated when callbacks change
 */

export interface HeatColorOption {
  id: string;
  label: string;
}

export interface HeatConfigEntry {
  enabled: boolean;
  min: number | null;
  max: number | null;
}

export interface SpanVizConfig {
  enabled: boolean;
  style: string;
  metric: string;
  color?: string;
}

export interface NumberFormat {
  group: string;
  decimal: string;
}

export interface ProfilerGridCallbacks {
  toggleChain?: (rowId: string | number) => void;
  expandChainTo?: (segments: Array<{ id?: string | number }>, segmentIndex: number) => void;
  focusNode?: (rowId: string | number) => void;
  getSpanVizValue?: (row: Record<string, unknown>) => { level: number; pct: number } | null;
  spanVizConfig?: SpanVizConfig;
  getHeatBounds?: (colKey: string) => { min: number; max: number; enabled: boolean };
  getColumnValue?: (colKey: string, row: Record<string, unknown>) => number | null;
  heatColorByKey?: Record<string, string>;
  heatStyleByKey?: Record<string, string>;
  numberFormat?: NumberFormat;
  heatConfig?: Record<string, HeatConfigEntry>;
  toggleHeat?: (colKey: string) => void;
  setHeatMin?: (colKey: string, val: number | null) => void;
  setHeatMax?: (colKey: string, val: number | null) => void;
  setHeatStyleForKey?: (colKey: string, style: string) => void;
  setHeatColorForKey?: (colKey: string, color: string) => void;
  heatColorOptions?: HeatColorOption[];
  minWallFilterMs?: string | number | null;
  setMinWallFilterMs?: (val: string) => void;
  defaultHeatStyle?: string;
  defaultHeatColor?: string;
  setSpanVizConfig?: (config: SpanVizConfig) => void;
  metricOptions?: { id: string; label: string }[];
}

export interface ProfilerGridContextValue {
  toggleChain: (rowId: string | number) => void;
  expandChainTo: (segments: Array<{ id?: string | number }>, segmentIndex: number) => void;
  focusNode: (rowId: string | number) => void;
  getSpanVizValue: (row: Record<string, unknown>) => { level: number; pct: number } | null;
  readonly spanVizConfig: SpanVizConfig | undefined;
  getHeatBounds: (colKey: string) => { min: number; max: number; enabled: boolean } | undefined;
  getColumnValue: (colKey: string, row: Record<string, unknown>) => number | null | undefined;
  readonly heatColorByKey: Record<string, string> | undefined;
  readonly heatStyleByKey: Record<string, string> | undefined;
  readonly numberFormat: NumberFormat | undefined;
  readonly heatConfig: Record<string, HeatConfigEntry> | undefined;
  toggleHeat: (colKey: string) => void;
  setHeatMin: (colKey: string, val: number | null) => void;
  setHeatMax: (colKey: string, val: number | null) => void;
  setHeatStyleForKey: (colKey: string, style: string) => void;
  setHeatColorForKey: (colKey: string, color: string) => void;
  readonly heatColorOptions: HeatColorOption[] | undefined;
  readonly minWallFilterMs: string | number | null | undefined;
  setMinWallFilterMs: (val: string) => void;
  readonly defaultHeatStyle: string | undefined;
  readonly defaultHeatColor: string | undefined;
  setSpanVizConfig: (config: SpanVizConfig) => void;
  readonly metricOptions?: { id: string; label: string }[];
}

const ProfilerGridContext = createContext<ProfilerGridContextValue | null>(null);

interface ProfilerGridProviderProps {
  children: React.ReactNode;
  callbacks: ProfilerGridCallbacks;
}

export function ProfilerGridProvider({ children, callbacks }: ProfilerGridProviderProps) {
  // Store callbacks in a ref to avoid triggering re-renders
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

  // Create stable wrapper functions that read from the ref
  // These are called during render, so they need to return the current value
  const stableApi = useRef<ProfilerGridContextValue>({
    // SpanCell callbacks
    toggleChain: (rowId: string | number) => callbacksRef.current.toggleChain?.(rowId),
    expandChainTo: (segments: Array<{ id?: string | number }>, segmentIndex: number) => callbacksRef.current.expandChainTo?.(segments, segmentIndex),
    focusNode: (rowId: string | number) => callbacksRef.current.focusNode?.(rowId),
    getSpanVizValue: (row: Record<string, unknown>) => callbacksRef.current.getSpanVizValue?.(row) ?? null,
    get spanVizConfig() {
      return callbacksRef.current.spanVizConfig;
    },
    
    // HeatCell callbacks
    getHeatBounds: (colKey: string) => callbacksRef.current.getHeatBounds?.(colKey),
    getColumnValue: (colKey: string, row: Record<string, unknown>) => callbacksRef.current.getColumnValue?.(colKey, row),
    get heatColorByKey() {
      return callbacksRef.current.heatColorByKey;
    },
    get heatStyleByKey() {
      return callbacksRef.current.heatStyleByKey;
    },
    get numberFormat() {
      return callbacksRef.current.numberFormat;
    },
    
    // Heat header configuration callbacks
    get heatConfig() {
      return callbacksRef.current.heatConfig;
    },
    toggleHeat: (colKey: string) => callbacksRef.current.toggleHeat?.(colKey),
    setHeatMin: (colKey: string, val: number | null) => callbacksRef.current.setHeatMin?.(colKey, val),
    setHeatMax: (colKey: string, val: number | null) => callbacksRef.current.setHeatMax?.(colKey, val),
    setHeatStyleForKey: (colKey: string, style: string) => callbacksRef.current.setHeatStyleForKey?.(colKey, style),
    setHeatColorForKey: (colKey: string, color: string) => callbacksRef.current.setHeatColorForKey?.(colKey, color),
    get heatColorOptions() {
      return callbacksRef.current.heatColorOptions;
    },
    get minWallFilterMs() {
      return callbacksRef.current.minWallFilterMs;
    },
    setMinWallFilterMs: (val: string) => callbacksRef.current.setMinWallFilterMs?.(val),
    get defaultHeatStyle() {
      return callbacksRef.current.defaultHeatStyle;
    },
    get defaultHeatColor() {
      return callbacksRef.current.defaultHeatColor;
    },
    
    // Span header configuration callbacks
    setSpanVizConfig: (config: SpanVizConfig) => callbacksRef.current.setSpanVizConfig?.(config),
  });

  return (
    <ProfilerGridContext.Provider value={stableApi.current}>
      {children}
    </ProfilerGridContext.Provider>
  );
}

export function useProfilerGridContext() {
  const context = useContext(ProfilerGridContext);
  if (!context) {
    throw new Error("useProfilerGridContext must be used within a ProfilerGridProvider");
  }
  return context;
}

export default ProfilerGridContext;
