import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import {
  Loader2,
  Filter,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { FilterBuilder } from "@/components/ui/filter-builder";
import type { FilterFieldDef, FilterGroupValue } from "@/components/ui/filter-builder";
import {
  getTransactions,
  getTransactionsAutocomplete,
  getTransactionsMaxes,
  getTxTypes,
} from "@/lib/api.ts";
import { TxContextHeatCell, TxContextHeatHeaderCell, TxContextActionCell } from "./HeatCells";
import { TransactionsGridProvider } from "../contexts/TransactionsGridContext";
import type { TransactionsGridCallbacks } from "../contexts/TransactionsGridContext";
import type { HeatConfigEntry } from "../contexts/ProfilerGridContext";
import { HEAT_COLOR_OPTIONS } from "../profilerConfig.ts";

// Buffer size - how many rows to fetch beyond visible area
const FETCH_BUFFER = 100;
// Minimum rows to fetch per request
const MIN_FETCH_SIZE = 200;

function truncateHash(hash: string | null | undefined, len = 12): string {
  if (!hash) return "-";
  if (hash.length <= len * 2) return hash;
  return `${hash.slice(0, len)}…${hash.slice(-len)}`;
}

// Cell component for transaction hash
function TxHashCell({ row }: { row: Record<string, any> }) {
  return (
    <span className="tx-hash-text numeric-text" title={row.tx_hash_hex}>
      {row.tx_hash_hex || "-"}
    </span>
  );
}

const DEFAULT_HEAT_MAXES: Record<string, number> = {
  duration_ms: 0,
  clarity_runtime: 0,
  clarity_read_count: 0,
  clarity_read_length: 0,
  clarity_write_count: 0,
  clarity_write_length: 0,
};

/** FilterBuilder field definitions — all filterable columns. */
const FILTER_FIELDS = [
  { id: "tx_type_name", label: "Transaction Type", type: "enum", enumValues: [] },
  { id: "contract_issuer", label: "Issuer / Principal", type: "text" },
  { id: "contract_name", label: "Contract", type: "text" },
  { id: "contract_fn", label: "Function", type: "text" },
  { id: "tx_hash_hex", label: "Transaction Hash", type: "text" },
  { id: "stacks_block_height", label: "Block Height", type: "number" },
  { id: "duration_ms", label: "Duration", type: "number", modifier: "duration" },
  { id: "clarity_runtime", label: "Clarity Runtime", type: "number" },
  { id: "clarity_read_count", label: "Read Count", type: "number" },
  { id: "clarity_read_length", label: "Read Length", type: "number" },
  { id: "clarity_write_count", label: "Write Count", type: "number" },
  { id: "clarity_write_length", label: "Write Length", type: "number" },
];

// ---------------------------------------------------------------------------
// Convert FilterBuilder state → MongoDB-style filter DSL sent to the server
// ---------------------------------------------------------------------------

/**
 * Map a single FilterBuilder operator id to its MongoDB-style DSL operator key.
 * Returns null for operators that need special per-value handling (multi-select).
 */
interface FilterRule {
  field: string;
  operator: string;
  value?: string | number | null;
  values?: string[];
  modifier?: string;
  rules?: FilterRule[];
  glue?: string;
}

interface FilterState {
  glue: "and" | "or";
  rules: (FilterRule | FilterGroupValue)[];
}

function opToDSL(operator: string): string {
  switch (operator) {
    case "contains":      return "$contains";
    case "notContains":   return "$ncontains";
    case "equal":         return "$eq";
    case "notEqual":      return "$ne";
    case "beginsWith":    return "$startsWith";
    case "endsWith":      return "$endsWith";
    case "greater":       return "$gt";
    case "greaterOrEqual":return "$gte";
    case "less":          return "$lt";
    case "lessOrEqual":   return "$lte";
    default:              return "$eq";
  }
}

/** Map FilterBuilder operator ids to MongoDB-style DSL operators. */
function ruleToDSL(rule: FilterRule): Record<string, unknown> | null {
  const { field, operator, value, values, modifier } = rule;

  // Multi-select: generate per-value clauses preserving operator semantics.
  // "equals" / enum "is" with multiple values → $in (efficient SQL IN(…)).
  // Enum "isNot" with multiple values → $nin (SQL NOT IN(…)).
  // Pattern ops (contains/beginsWith/endsWith) → $or of individual clauses.
  if (values && values.length > 0) {
    if (operator === "equal" || operator === "is") {
      return { [field]: { $in: values } };
    }
    if (operator === "isNot") {
      return { [field]: { $nin: values } };
    }
    const dslOp = opToDSL(operator);
    // notContains with multiple values → all must NOT match → $and
    const combinator = operator === "notContains" ? "$and" : "$or";
    const clauses = values.map((v) => ({ [field]: { [dslOp]: v } }));
    return clauses.length === 1 ? clauses[0] : { [combinator]: clauses };
  }

  if (value == null || value === "") return null;

  /** Apply duration modifier to convert the user-entered value to ms. */
  const applyDurationMod = (v: string | number) => {
    const n = Number(v);
    if (modifier === "s") return n * 1000;
    if (modifier === "us") return n / 1000;
    return n; // ms (default)
  };

  // Determine the right MongoDB-style operator + value
  let dslOp, dslVal;
  switch (operator) {
    case "contains":
      dslOp = "$contains";
      dslVal = value;
      break;
    case "notContains":
      dslOp = "$ncontains";
      dslVal = value;
      break;
    case "equal":
      dslOp = "$eq";
      dslVal = value;
      break;
    case "notEqual":
      dslOp = "$ne";
      dslVal = value;
      break;
    case "beginsWith":
      dslOp = "$startsWith";
      dslVal = value;
      break;
    case "endsWith":
      dslOp = "$endsWith";
      dslVal = value;
      break;
    case "greater":
      dslOp = "$gt";
      dslVal = applyDurationMod(value);
      break;
    case "greaterOrEqual":
      dslOp = "$gte";
      dslVal = applyDurationMod(value);
      break;
    case "less":
      dslOp = "$lt";
      dslVal = applyDurationMod(value);
      break;
    case "lessOrEqual":
      dslOp = "$lte";
      dslVal = applyDurationMod(value);
      break;
    default:
      dslOp = "$eq";
      dslVal = value;
  }

  return { [field]: { [dslOp]: dslVal } };
}

/**
 * Recursively convert FilterBuilder `{ glue, rules }` into the MongoDB-style
 * DSL accepted by the backend `parseFilterParam()`.
 *
 * Returns `null` when there are no meaningful conditions.
 */
function filterStateToDSL(state: FilterState | null): Record<string, unknown> | null {
  if (!state?.rules?.length) return null;

  const clauses = [];
  for (const rule of state.rules) {
    if ("rules" in rule && Array.isArray(rule.rules)) {
      // Nested group
      const nested = filterStateToDSL(rule as FilterState);
      if (nested) clauses.push(nested);
    } else {
      const dsl = ruleToDSL(rule as FilterRule);
      if (dsl) clauses.push(dsl);
    }
  }

  if (clauses.length === 0) return null;
  if (clauses.length === 1) return clauses[0];

  const combinator = state.glue === "or" ? "$or" : "$and";
  return { [combinator]: clauses };
}

interface NumberFormatOption {
  id: string;
  label: string;
  group: string;
  decimal: string;
}

interface TransactionsTabProps {
  runId: string;
  onViewTrace: (txHashHex: string, stacksTxId: number) => void;
  numberFormat: NumberFormatOption;
  savedState: React.MutableRefObject<Record<string, any> | null>;
}

export default function TransactionsTab({ runId, onViewTrace, numberFormat, savedState }: TransactionsTabProps) {
  // Restore state from savedState ref (survives unmount/remount across tab switches)
  const restored = savedState?.current;

  // Grid data - sparse array indexed by row position
  const [dataCache, setDataCache] = useState<Record<string, any>[]>(() => {
    if (restored?.cachedRows) {
      const arr: Record<string, any>[] = [];
      for (const [idx, row] of restored.cachedRows) arr[idx] = row;
      return arr;
    }
    return [];
  });
  // Current visible range for the grid (svar-ui expects only visible slice in data prop)
  const [visibleRange, setVisibleRange] = useState(() => {
    if (restored?.activeRowIndex != null && restored?.cachedTotal > 0) {
      // Center the view around the row we navigated from
      const center = restored.activeRowIndex;
      const start = Math.max(0, center - 25);
      const end = Math.min(restored.cachedTotal, center + 25);
      return { start, end };
    }
    return { start: 0, end: MIN_FETCH_SIZE };
  });
  const [total, setTotal] = useState(restored?.cachedTotal ?? 0);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sortBy, setSortBy] = useState(restored?.sortBy ?? "duration_ms");
  const [sortDir, setSortDir] = useState(restored?.sortDir ?? "desc");
  const [filterValue, setFilterValue] = useState<FilterState>(restored?.filterValue ?? { glue: "and", rules: [] });
  const [showFilters, setShowFilters] = useState(restored?.showFilters ?? true);
  const abortControllerRef = useRef<AbortController | null>(null);
  const fetchedRangesRef = useRef(
    restored?.cachedRows
      ? (() => {
          const indices = restored.cachedRows.map(([i]: [number, ...any[]]) => i);
          return [{ start: Math.min(...indices), end: Math.max(...indices) + 1 }];
        })()
      : []
  );
  const gridApiRef = useRef<Record<string, any> | null>(null);
  // Track last request to avoid duplicates
  const lastRequestRef = useRef({ start: 0, end: 0 });
  // Row index to scroll to and select after remount
  const pendingScrollRef = useRef<number | null>(restored?.activeRowIndex ?? null);
  const pendingSelectIdRef = useRef<string | null>(restored?.activeRowId ?? null);

  // Transaction type enum values (fetched once)
  const [txTypes, setTxTypes] = useState<string[]>([]);

  // Fetch transaction types on mount
  useEffect(() => {
    let cancelled = false;
    getTxTypes().then((data) => {
      if (!cancelled && Array.isArray(data)) {
        setTxTypes(data.map((t) => t.name));
      }
    }).catch(() => {});
    return () => { cancelled = true; };
  }, []);

  // Enrich FILTER_FIELDS with fetched enum values
  const filterFields = useMemo(
    () =>
      FILTER_FIELDS.map((f) =>
        f.id === "tx_type_name" ? { ...f, enumValues: txTypes } : f
      ),
    [txTypes]
  );
  
  // Heat configuration state (per-column min/max/enabled)
  const [heatConfig, setHeatConfig] = useState<Record<string, HeatConfigEntry>>(restored?.heatConfig ?? {});
  // Per-column heat style and color
  const [heatStyleByKey, setHeatStyleByKey] = useState<Record<string, string>>(restored?.heatStyleByKey ?? {});
  const [heatColorByKey, setHeatColorByKey] = useState<Record<string, string>>(restored?.heatColorByKey ?? {});
  // Min wall (duration) filter - shown in duration_ms header settings
  const [minWallFilterMs, setMinWallFilterMs] = useState(restored?.minWallFilterMs ?? "");
  
  // Heat config handlers
  const toggleHeat = useCallback((key: string) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { ...prev[key], enabled: !(prev[key]?.enabled ?? false), min: prev[key]?.min ?? null, max: prev[key]?.max ?? null }
    }));
  }, []);
  
  const setHeatMin = useCallback((key: string, val: string | number | null) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { enabled: prev[key]?.enabled ?? false, max: prev[key]?.max ?? null, min: val === "" || val === null ? null : Number(val) }
    }));
  }, []);
  
  const setHeatMax = useCallback((key: string, val: string | number | null) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { enabled: prev[key]?.enabled ?? false, min: prev[key]?.min ?? null, max: val === "" || val === null ? null : Number(val) }
    }));
  }, []);
  
  const setHeatStyleForKey = useCallback((key: string, style: string) => {
    setHeatStyleByKey(prev => ({ ...prev, [key]: style }));
  }, []);
  
  const setHeatColorForKey = useCallback((key: string, color: string) => {
    setHeatColorByKey(prev => ({ ...prev, [key]: color }));
  }, []);

  const [heatMaxes, setHeatMaxes] = useState<Record<string, number>>(restored?.cachedHeatMaxes ?? DEFAULT_HEAT_MAXES);

  // ── Persist restorable state to the parent ref on unmount ──────────────
  // We use refs to avoid re-running the effect on every state change.
  const stateSnapshotRef = useRef<Record<string, any>>({});
  useEffect(() => {
    stateSnapshotRef.current = {
      sortBy, sortDir, filterValue, showFilters,
      heatConfig, heatStyleByKey, heatColorByKey, minWallFilterMs,
    };
  });
  useEffect(() => {
    return () => {
      // On unmount, write the latest snapshot into the parent-owned ref
      if (savedState) {
        savedState.current = stateSnapshotRef.current;
      }
    };
  }, [savedState]);

  /**
   * Combine the FilterBuilder state + the heat-header min-duration into a
   * single MongoDB-style DSL object that is sent as `?filter=…` to the API.
   */
  const filterDSL = useMemo(() => {
    const base = filterStateToDSL(filterValue);
    const minMs = minWallFilterMs ? Number(minWallFilterMs) : 0;
    if (minMs <= 0) return base;

    // Extra clause: duration_ms >= minWallFilterMs
    const extra = { duration_ms: { $gte: minMs } };
    if (!base) return extra;
    // Wrap both under $and
    return { $and: [base, extra] };
  }, [filterValue, minWallFilterMs]);

  // Build stable column definitions for svar-ui Grid.
  // These use context-aware components that fetch their configuration at render time,
  // allowing the column array to remain stable and preserve column positions/widths.
  const columns = useMemo(() => {
    // Helper to build a heat header with context-aware component
    const makeHeatHeader = (colKey: string, text: string, isWallColumn = false) => ({
      text,
      cell: TxContextHeatHeaderCell,
    });
    
    return [
      {
        id: "row_number",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "#" },
        ],
        width: 60,
        resize: true,
        css: "grid-col-numeric grid-col-dimmed",
        template: (val: any) => (val == null ? "-" : String(val)),
      },
      {
        id: "_actions",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "" },
        ],
        width: 50,
        cell: TxContextActionCell,
      },
      {
        id: "tx_hash_hex",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Transaction Hash" },
        ],
        width: 420,
        sort: true,
        resize: true,
        cell: TxHashCell,
      },
      {
        id: "contract_issuer",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Issuer" },
        ],
        width: 140,
        resize: true,
        template: (val: any) => val ? truncateHash(val, 6) : "-",
        css: (row: any) => row.contract_issuer ? "" : "dimmed-cell",
      },
      {
        id: "contract_name",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Contract" },
        ],
        width: 160,
        resize: true,
        template: (val: any) => val || "-",
        css: (row: any) => row.contract_name ? "" : "dimmed-cell",
      },
      {
        id: "contract_fn",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Function" },
        ],
        width: 140,
        resize: true,
        template: (val: any) => val || "-",
        css: (row: any) => row.contract_fn ? "" : "dimmed-cell",
      },
      {
        id: "tx_type_name",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Type" },
        ],
        width: 160,
        sort: true,
        resize: true,
        template: (val: any) => val || "-",
        css: (row: any) => row.tx_type_name ? "" : "dimmed-cell",
      },
      {
        id: "duration_ms",
        _colKey: "duration_ms",
        _decimals: 2,
        _isWallColumn: true,
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          makeHeatHeader("duration_ms", "Duration (ms)", true),
        ],
        width: 130,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "stacks_block_height",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Block" },
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        template: (val: any) => (val == null ? "-" : String(val)),
      },
      // ═══════════════════════════════════════════════════════════════════════════
      // Clarity - 5 columns: Runtime, Read (Count, Length), Write (Count, Length)
      // ═══════════════════════════════════════════════════════════════════════════
      {
        id: "clarity_runtime",
        _colKey: "clarity_runtime",
        _decimals: 0,
        header: [
          { text: "Clarity", colspan: 5, css: "grid-group-header grid-level1-header" },
          { text: "", css: "grid-level2-empty" },
          makeHeatHeader("clarity_runtime", "Runtime"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_read_count",
        _colKey: "clarity_read_count",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "Read", colspan: 2, css: "grid-group-header grid-level2-header" },
          makeHeatHeader("clarity_read_count", "Count"),
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_read_length",
        _colKey: "clarity_read_length",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "", _hidden: true },
          makeHeatHeader("clarity_read_length", "Length"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_write_count",
        _colKey: "clarity_write_count",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "Write", colspan: 2, css: "grid-group-header grid-level2-header" },
          makeHeatHeader("clarity_write_count", "Count"),
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_write_length",
        _colKey: "clarity_write_length",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "", _hidden: true },
          makeHeatHeader("clarity_write_length", "Length"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
    ];
  }, []); // No dependencies - columns are now stable!

  const heatMaxAbortRef = useRef<AbortController | null>(null);
  const [heatMaxesLoaded, setHeatMaxesLoaded] = useState(false);

  useEffect(() => {
    if (!runId) {
      setHeatMaxes(DEFAULT_HEAT_MAXES);
      setHeatMaxesLoaded(true);
      return;
    }

    if (heatMaxAbortRef.current) {
      heatMaxAbortRef.current.abort();
    }
    const controller = new AbortController();
    heatMaxAbortRef.current = controller;

    const fetchHeatMaxes = async () => {
      try {
        setHeatMaxesLoaded(false);
        const params: Record<string, any> = { run_id: runId };
        if (filterDSL) params.filter = filterDSL;
        const data: { maxes?: Record<string, number> } = await getTransactionsMaxes(params, { signal: controller.signal });
        setHeatMaxes({ ...DEFAULT_HEAT_MAXES, ...(data.maxes || {}) });
      } catch (err) {
        if ((err as Error).name !== "AbortError") {
          setHeatMaxes(DEFAULT_HEAT_MAXES);
        }
      } finally {
        setHeatMaxesLoaded(true);
      }
    };

    fetchHeatMaxes();

    return () => {
      controller.abort();
    };
  }, [runId, filterDSL]);

  // Fetch a range of data from the server
  const fetchRange = useCallback(async (start: number, end: number) => {
    if (!runId) return;

    // Expand range to include buffer
    const fetchStart = Math.max(0, start - FETCH_BUFFER);
    const fetchEnd = end + FETCH_BUFFER;
    const fetchSize = Math.max(MIN_FETCH_SIZE, fetchEnd - fetchStart);

    // Create a range key to track what we've fetched
    const fetchEndInclusive = fetchStart + fetchSize;
    const alreadyFetched = fetchedRangesRef.current.some(
      (range) => fetchStart >= range.start && fetchEndInclusive <= range.end
    );
    if (alreadyFetched) {
      return; // Already fetched this range
    }

    // Cancel any existing request
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }

    const controller = new AbortController();
    abortControllerRef.current = controller;

    setIsLoading(true);

    try {
      const params: Record<string, any> = {
        run_id: runId,
        offset: fetchStart,
        limit: fetchSize,
        sort_by: sortBy,
        sort_dir: sortDir,
      };
      if (filterDSL) params.filter = filterDSL;

      const data: { total: number; rows: Record<string, any>[] } = await getTransactions(params, { signal: controller.signal });
      
      // Update total count
      setTotal(data.total);
      
      // Mark this range as fetched
      fetchedRangesRef.current.push({ start: fetchStart, end: fetchEndInclusive });

      // Merge new data into cache (sparse array)
      setDataCache((prev) => {
        const next = [...prev];
        data.rows.forEach((row, i) => {
          const index = fetchStart + i;
          // Add unique id for the grid
          next[index] = {
            ...row,
            id: `${row.stacks_tx_id}-${row.synthetic_block_id}`,
            row_number: index + 1,
          };
        });
        return next;
      });

      setError(null);
    } catch (err) {
      if ((err as Error).name !== "AbortError") {
        setError((err as Error).message);
      }
    } finally {
      setIsLoading(false);
      abortControllerRef.current = null;
    }
  }, [runId, sortBy, sortDir, filterDSL]);

  // Reset cache when filters/sort/run changes
  const isRestoringRef = useRef(!!restored?.cachedRows);
  useEffect(() => {
    // Skip the first reset if we're restoring from saved state
    if (isRestoringRef.current) {
      isRestoringRef.current = false;
      return;
    }
    setDataCache([]);
    setTotal(0);
    setVisibleRange({ start: 0, end: MIN_FETCH_SIZE });
    fetchedRangesRef.current = [];
    lastRequestRef.current = { start: 0, end: 0 };
    
    if (runId) {
      // Fetch initial data
      fetchRange(0, MIN_FETCH_SIZE);
    }
  }, [runId, sortBy, sortDir, filterDSL, fetchRange]);

  // Handle dynamic data request from grid during scroll
  const handleRequestData = useCallback((ev: { row?: { start: number; end: number } }) => {
    const { row } = ev;
    if (row) {
      const { start, end } = row;
      
      // Update visible range for rendering the correct slice
      // (functional update avoids a re-render when only horizontal scroll fires)
      setVisibleRange((prev) =>
        prev.start === start && prev.end === end ? prev : { start, end }
      );
      
      // Avoid duplicate requests for same range
      if (start === lastRequestRef.current.start && end === lastRequestRef.current.end) {
        return;
      }
      lastRequestRef.current = { start, end };

      const rangeCovered = fetchedRangesRef.current.some(
        (range) => start >= range.start && end <= range.end
      );
      if (!rangeCovered) {
        fetchRange(start, end);
      }
    }
  }, [fetchRange]);

  const clearFilters = useCallback(() => {
    setFilterValue({ glue: "and", rules: [] });
  }, []);

  const hasActiveFilters = useMemo(() => {
    return filterValue.rules?.length > 0;
  }, [filterValue]);

  const handleFilterChange = useCallback((newValue: FilterState) => {
    setFilterValue(newValue);
  }, []);

  // Async autocomplete provider — called on every keystroke (debounced
  // inside the FilterBuilder) with the current search text and an
  // AbortSignal so in-flight requests are cancelled automatically.
  const filterOptions = useCallback(async (field: string, query: string, signal: AbortSignal) => {
    if (!runId) return [];
    try {
      const params = {
        run_id: runId,
        field,
        q: query || "",
        limit: 50,
      };
      // Intentionally NOT passing filterDSL — autocomplete should show all
      // possible values regardless of other active filters, since the user
      // may still add/remove those filters.
      const data = await getTransactionsAutocomplete(params, { signal }) as { values?: string[] };
      return data.values || [];
    } catch (e) {
      if ((e as Error).name === "AbortError") throw e;
      return [];
    }
  }, [runId]);

  // Grid init callback — schedules scroll + select restoration if returning to this tab
  const handleInit = useCallback((api: Record<string, any>) => {
    gridApiRef.current = api;

    // Scroll to the row we navigated from and select it
    if (pendingScrollRef.current != null && pendingScrollRef.current > 0) {
      const targetRow = pendingScrollRef.current;
      const selectId = pendingSelectIdRef.current;
      pendingScrollRef.current = null;
      pendingSelectIdRef.current = null;
      requestAnimationFrame(() => {
        const el = document.querySelector(".transactions-grid-container .wx-scroll");
        if (el) {
          el.scrollTop = targetRow * 36; // row height = 36px
        }
        if (selectId) {
          api.exec("select-row", { id: selectId });
        }
      });
    }
  }, []);

  const hasAnyData = useMemo(() => dataCache.some(Boolean), [dataCache]);

  // Compute visible data slice for the grid (svar-ui expects only the visible rows)
  const visibleData = useMemo(() => {
    const { start, end } = visibleRange;
    return dataCache.slice(start, end);
  }, [dataCache, visibleRange]);

  // Build callbacks object for the context provider.
  // These callbacks are accessed via context to avoid recreating column definitions.
  const handleViewTrace = useCallback((row: Record<string, any>) => {
    // Snapshot the row's neighborhood into savedState so we can restore on return
    if (savedState) {
      const rowIndex = dataCache.indexOf(row);
      const idx = rowIndex >= 0 ? rowIndex : dataCache.findIndex(r => r && r.id === row.id);
      if (idx >= 0) {
        const lo = Math.max(0, idx - 50);
        const hi = Math.min(dataCache.length, idx + 51);
        const entries = [];
        for (let i = lo; i < hi; i++) {
          if (dataCache[i]) entries.push([i, dataCache[i]]);
        }
        // Merge into the current snapshot (filters/sort/heat already tracked)
        Object.assign(stateSnapshotRef.current, {
          cachedRows: entries,
          cachedTotal: total,
          cachedHeatMaxes: heatMaxes,
          activeRowIndex: idx,
          activeRowId: row.id,
        });
        savedState.current = stateSnapshotRef.current;
      }
    }
    onViewTrace(row.tx_hash_hex, row.stacks_tx_id);
  }, [onViewTrace, dataCache, total, heatMaxes, savedState]);

  const gridCallbacks = {
    onViewTrace: handleViewTrace,
    heatConfig,
    heatMaxes,
    heatStyleByKey,
    heatColorByKey,
    numberFormat,
    toggleHeat,
    setHeatMin,
    setHeatMax,
    setHeatStyleForKey,
    setHeatColorForKey,
    heatColorOptions: HEAT_COLOR_OPTIONS,
    minWallFilterMs,
    setMinWallFilterMs,
  };

  return (
    <div className="transactions-tab">
      {/* Toolbar */}
      <div className="transactions-toolbar">
        <div className="transactions-toolbar-left">
          <Button
            variant={showFilters ? "default" : "outline"}
            size="sm"
            className="gap-2"
            onClick={() => setShowFilters(!showFilters)}
          >
            <Filter className="h-4 w-4" />
            Filters
            {hasActiveFilters && (
              <span className="transactions-filter-badge">•</span>
            )}
          </Button>
          {hasActiveFilters && (
            <Button
              variant="ghost"
              size="sm"
              className="gap-1 text-muted-foreground"
              onClick={clearFilters}
            >
              <X className="h-3 w-3" />
              Clear
            </Button>
          )}
        </div>
        <div className="transactions-toolbar-right">
          <span className="transactions-count">
            {isLoading ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              `${total.toLocaleString()} transactions`
            )}
          </span>
        </div>
      </div>

      {/* Filters Panel */}
      {showFilters && (
        <div className="transactions-filters">
          <FilterBuilder
            fields={filterFields as FilterFieldDef[]}
            options={filterOptions}
            value={filterValue as FilterGroupValue}
            onChange={handleFilterChange as (v: FilterGroupValue) => void}
          />
        </div>
      )}

      {/* Grid Container */}
      <div className="transactions-grid-container">
        {error ? (
          <div className="transactions-error">
            <span>Error: {error}</span>
            <Button variant="outline" size="sm" onClick={() => fetchRange(0, MIN_FETCH_SIZE)}>
              Retry
            </Button>
          </div>
        ) : (
          <WillowDark>
            <TransactionsGridProvider callbacks={gridCallbacks as TransactionsGridCallbacks}>
              <Grid
                data={visibleData}
                columns={columns as any}
                sizes={{ rowHeight: 36 }}
                dynamic={total > 0 ? { rowCount: total } : undefined}
                onRequestData={handleRequestData}
                init={handleInit}
                columnStyle={(column: any) => column.css || ""}
                overlay={isLoading && !hasAnyData || !heatMaxesLoaded ? "Loading transactions..." : undefined}
              />
            </TransactionsGridProvider>
          </WillowDark>
        )}
      </div>
    </div>
  );
}
