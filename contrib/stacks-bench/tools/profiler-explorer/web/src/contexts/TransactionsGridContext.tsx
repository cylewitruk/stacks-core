import React, { createContext, useContext, useRef } from "react";
import type { HeatConfigEntry, HeatColorOption, NumberFormat } from "./ProfilerGridContext";

/**
 * Context for transactions grid callbacks and configuration.
 * 
 * This context provides stable references to callbacks and config used by grid cell components.
 * By using refs internally, we ensure that:
 * 1. The context value remains stable (doesn't trigger re-renders of column definitions)
 * 2. Cell components always call the latest version of callbacks
 * 3. The columns array doesn't need to be recreated when callbacks change
 */

export interface TransactionsGridCallbacks {
  onViewTrace?: (row: Record<string, unknown>) => void;
  getHeatPercent?: (colKey: string, value: number) => number;
  heatConfig?: Record<string, HeatConfigEntry>;
  heatMaxes?: Record<string, number>;
  heatStyleByKey?: Record<string, string>;
  heatColorByKey?: Record<string, string>;
  numberFormat?: NumberFormat;
  toggleHeat?: (colKey: string) => void;
  setHeatMin?: (colKey: string, val: number | null) => void;
  setHeatMax?: (colKey: string, val: number | null) => void;
  setHeatStyleForKey?: (colKey: string, style: string) => void;
  setHeatColorForKey?: (colKey: string, color: string) => void;
  heatColorOptions?: HeatColorOption[];
  minWallFilterMs?: number | null;
  setMinWallFilterMs?: (val: number | null) => void;
}

export interface TransactionsGridContextValue {
  onViewTrace: (row: Record<string, unknown>) => void;
  getHeatPercent: (colKey: string, value: number) => number | undefined;
  readonly heatConfig: Record<string, HeatConfigEntry> | undefined;
  readonly heatMaxes: Record<string, number> | undefined;
  readonly heatStyleByKey: Record<string, string> | undefined;
  readonly heatColorByKey: Record<string, string> | undefined;
  readonly numberFormat: NumberFormat | undefined;
  toggleHeat: (colKey: string) => void;
  setHeatMin: (colKey: string, val: number | null) => void;
  setHeatMax: (colKey: string, val: number | null) => void;
  setHeatStyleForKey: (colKey: string, style: string) => void;
  setHeatColorForKey: (colKey: string, color: string) => void;
  readonly heatColorOptions: HeatColorOption[] | undefined;
  readonly minWallFilterMs: number | null | undefined;
  setMinWallFilterMs: (val: number | null) => void;
}

const TransactionsGridContext = createContext<TransactionsGridContextValue | null>(null);

interface TransactionsGridProviderProps {
  children: React.ReactNode;
  callbacks: TransactionsGridCallbacks;
}

export function TransactionsGridProvider({ children, callbacks }: TransactionsGridProviderProps) {
  // Store callbacks in a ref to avoid triggering re-renders
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

  // Create stable wrapper functions that read from the ref
  const stableApi = useRef<TransactionsGridContextValue>({
    // View trace callback
    onViewTrace: (row: Record<string, unknown>) => callbacksRef.current.onViewTrace?.(row),
    
    // Heat cell callbacks
    getHeatPercent: (colKey: string, value: number) => callbacksRef.current.getHeatPercent?.(colKey, value),
    get heatConfig() {
      return callbacksRef.current.heatConfig;
    },
    get heatMaxes() {
      return callbacksRef.current.heatMaxes;
    },
    get heatStyleByKey() {
      return callbacksRef.current.heatStyleByKey;
    },
    get heatColorByKey() {
      return callbacksRef.current.heatColorByKey;
    },
    get numberFormat() {
      return callbacksRef.current.numberFormat;
    },
    
    // Heat header configuration callbacks
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
    setMinWallFilterMs: (val: number | null) => callbacksRef.current.setMinWallFilterMs?.(val),
  });

  return (
    <TransactionsGridContext.Provider value={stableApi.current}>
      {children}
    </TransactionsGridContext.Provider>
  );
}

export function useTransactionsGridContext() {
  const context = useContext(TransactionsGridContext);
  if (!context) {
    throw new Error("useTransactionsGridContext must be used within a TransactionsGridProvider");
  }
  return context;
}

export default TransactionsGridContext;
