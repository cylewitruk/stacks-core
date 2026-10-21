import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import ProfilerGrid from "./components/ProfilerGrid";
import SettingsPanel from "./components/SettingsPanel";
import TransactionsTab from "./components/TransactionsTab";
import SpanDetailsModal from "./components/SpanDetailsModal";
import SpanCell from "./components/SpanCell";
import { ContextHeatCell, ContextNumericCell, ContextHeatHeaderCell, ContextSpanHeaderCell } from "./components/HeatCells";
import HeaderBar from "./components/HeaderBar";
import ToolbarBar from "./components/ToolbarBar";
import BreadcrumbBar from "./components/BreadcrumbBar";
import { ProfilerGridProvider } from "./contexts/ProfilerGridContext";
import type { HeatConfigEntry, SpanVizConfig } from "./contexts/ProfilerGridContext";
import {
  applyChainCompression,
  applyFocus,
  applyHotPath,
  applyOpenState,
  applyTreeFilter,
  buildTreeIndex,
  collectSubtreeIds,
  computeDefaultOpenSet,
  flattenTree,
  getSelfCpuUs,
  getSelfWaitUs,
  getSelfWallUs,
  getWallUs,
  isClarityPseudoContext,
  indexTree,
  pruneTree,
} from "./treeTransforms.ts";
import type { TreeNode, TreeFilterGroup } from "./treeTransforms.ts";
import {
  ALWAYS_HIDDEN_KEYS,
  ALWAYS_VISIBLE_KEYS,
  COLUMN_DEFS,
  COUNT_DIM_KEYS,
  DEFAULT_COLUMNS,
  NUMERIC_COLUMN_KEYS,
  sanitizeSelectedColumns,
} from "./columnsConfig.ts";
import type { ColumnDef } from "./columnsConfig.ts";
import {
  DEFAULT_AUTO_EXPAND,
  DEFAULT_HEAT_COLOR,
  DEFAULT_HEAT_STYLE,
  DEFAULT_NUMBER_FORMAT_ID,
  HEAT_COLOR_OPTIONS,
  NUMBER_FORMATS,
  THEME_PRESETS,
} from "./profilerConfig.ts";
import { getBlocks, getRuns, getTrace, lookupTx } from "./lib/api.ts";
import { FilterBuilder } from "./components/ui/filter-builder";
import type { FilterFieldDef, FilterGroupValue, RichOption } from "./components/ui/filter-builder";

// ---------------------------------------------------------------------------
// Trace grid filter field definitions (client-side filtering)
// ---------------------------------------------------------------------------

// Trace grid filter field definitions — static fields (don't depend on loaded data)
const TRACE_FILTER_FIELDS_STATIC: FilterFieldDef[] = [
  { id: "tag", label: "Tag", type: "text" },
  {
    id: "clarity_fn_type",
    label: "Function Type",
    type: "enum",
    enumValues: ["Built-In", "Public", "Private", "Read-Only"],
    group: "Clarity",
  },
  { id: "call_count", label: "Calls", type: "number" },

  // ── Wall Time ──
  { id: "wall_inc_total",  label: "Wall Inc. Total",  type: "number", modifier: "duration", group: "Wall Time" },
  { id: "wall_inc_avg",    label: "Wall Inc. Avg.",    type: "number", modifier: "duration", group: "Wall Time" },
  { id: "wall_self_total", label: "Wall Self Total",   type: "number", modifier: "duration", group: "Wall Time" },
  { id: "wall_self_avg",   label: "Wall Self Avg.",    type: "number", modifier: "duration", group: "Wall Time" },

  // ── Busy Time ──
  { id: "busy_inc_total",  label: "Busy Inc. Total",  type: "number", modifier: "duration", group: "Busy Time" },
  { id: "busy_inc_avg",    label: "Busy Inc. Avg.",    type: "number", modifier: "duration", group: "Busy Time" },
  { id: "busy_self_total", label: "Busy Self Total",   type: "number", modifier: "duration", group: "Busy Time" },
  { id: "busy_self_avg",   label: "Busy Self Avg.",    type: "number", modifier: "duration", group: "Busy Time" },

  // ── Wait Time ──
  { id: "wait_inc_total",  label: "Wait Inc. Total",  type: "number", modifier: "duration", group: "Wait Time" },
  { id: "wait_inc_avg",    label: "Wait Inc. Avg.",    type: "number", modifier: "duration", group: "Wait Time" },
  { id: "wait_self_total", label: "Wait Self Total",   type: "number", modifier: "duration", group: "Wait Time" },
  { id: "wait_self_avg",   label: "Wait Self Avg.",    type: "number", modifier: "duration", group: "Wait Time" },

  // ── Clarity numeric metrics ──
  { id: "clarity_runtime_total",      label: "Runtime Total",      type: "number", group: "Clarity" },
  { id: "clarity_runtime_avg",        label: "Runtime Avg.",       type: "number", group: "Clarity" },
  { id: "clarity_input_n_total",      label: "Cost Input (n) Total", type: "number", group: "Clarity" },
  { id: "clarity_input_n_avg",        label: "Cost Input (n) Avg.", type: "number", group: "Clarity" },
  { id: "clarity_read_count_total",   label: "Read Count Total",   type: "number", group: "Clarity" },
  { id: "clarity_read_count_avg",     label: "Read Count Avg.",    type: "number", group: "Clarity" },
  { id: "clarity_read_length_total",  label: "Read Length Total",  type: "number", group: "Clarity" },
  { id: "clarity_read_length_avg",    label: "Read Length Avg.",   type: "number", group: "Clarity" },
  { id: "clarity_write_count_total",  label: "Write Count Total",  type: "number", group: "Clarity" },
  { id: "clarity_write_count_avg",    label: "Write Count Avg.",   type: "number", group: "Clarity" },
  { id: "clarity_write_length_total", label: "Write Length Total", type: "number", group: "Clarity" },
  { id: "clarity_write_length_avg",   label: "Write Length Avg.",  type: "number", group: "Clarity" },
];

/** Operators available for Span Name: text ops + is/isNot. */
const SPAN_NAME_OPERATORS = [
  { id: "contains", label: "contains" },
  { id: "notContains", label: "does not contain" },
  { id: "equal", label: "equals" },
  { id: "notEqual", label: "does not equal" },
  { id: "beginsWith", label: "begins with" },
  { id: "endsWith", label: "ends with" },
  { id: "is", label: "is" },
  { id: "isNot", label: "is not" },
];

/** Operators available for Context: text ops + is/isNot. */
const CONTEXT_OPERATORS = [
  { id: "contains", label: "contains" },
  { id: "notContains", label: "does not contain" },
  { id: "equal", label: "equals" },
  { id: "notEqual", label: "does not equal" },
  { id: "beginsWith", label: "begins with" },
  { id: "endsWith", label: "ends with" },
  { id: "is", label: "is" },
  { id: "isNot", label: "is not" },
];



function getNumberFormat(id: string) {
  return NUMBER_FORMATS.find((format) => format.id === id) || NUMBER_FORMATS[2];
}

// Build a map of column key -> column definition for quick lookup
const COLUMN_BY_KEY = Object.fromEntries(COLUMN_DEFS.map((col) => [col.key, col]));

// Get all numeric columns that can have heatmaps
const HEAT_CAPABLE_COLUMNS = COLUMN_DEFS.filter((col) => NUMERIC_COLUMN_KEYS.has(col.key));

function buildDefaultHeatConfig(): Record<string, { enabled: boolean; min: null; max: null }> {
  const defaults: Record<string, { enabled: boolean; min: null; max: null }> = {};
  for (const col of HEAT_CAPABLE_COLUMNS) {
    defaults[col.key] = { enabled: false, min: null, max: null };
  }
  return defaults;
}

function buildDefaultHeatStyleMap(): Record<string, string> {
  const legacy = localStorage.getItem("profilerHeatStyle") || DEFAULT_HEAT_STYLE;
  const defaults: Record<string, string> = {};
  for (const col of HEAT_CAPABLE_COLUMNS) {
    defaults[col.key] = legacy;
  }
  return defaults;
}

function buildDefaultHeatColorMap(): Record<string, string> {
  const defaults: Record<string, string> = {};
  for (const col of HEAT_CAPABLE_COLUMNS) {
    defaults[col.key] = DEFAULT_HEAT_COLOR;
  }
  return defaults;
}

const DEFAULT_NUMERIC_WIDTH = 100;
const MIN_NUMERIC_WIDTH = 90;

function getSafeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

export default function App() {
  const [runs, setRuns] = useState<Array<{ id: number; run_name?: string }>>([]);
  const [blocks, setBlocks] = useState<Array<{ stacks_block_id: string; height: number; block_hash_hex?: string }>>([]);
  const [runId, setRunId] = useState("");
  const [activeTab, setActiveTab] = useState("transactions"); // "trace" | "transactions"
  const [mode, setMode] = useState("tx");
  const [txQuery, setTxQuery] = useState("");
  const [stacksTxId, setStacksTxId] = useState("");
  const [stacksBlockId, setStacksBlockId] = useState("");
  const [minWallMs, setMinWallMs] = useState("");
  const [segmentRootId, setSegmentRootId] = useState("");
  const [limit, setLimit] = useState("5000");
  const [rows, setRows] = useState<Record<string, any>[]>([]);
  const [summary, setSummary] = useState("");
  const [chainCompression, setChainCompression] = useState(true);
  const [hotPathMode, setHotPathMode] = useState<"off" | "self" | "total">("off");
  const [focusId, setFocusId] = useState<string | number | null>(null);
  const [activeId, setActiveId] = useState<string | number | null>(null);
  const [spanDetailsOpen, setSpanDetailsOpen] = useState(false);
  const [openNodes, setOpenNodes] = useState<Set<string | number>>(new Set());
  const [expandedChains, setExpandedChains] = useState<Set<string | number>>(new Set());
  const [traceFilter, setTraceFilter] = useState<TreeFilterGroup>({ glue: "and", rules: [] });
  const [traceFilterEnabled, setTraceFilterEnabled] = useState(true);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [theme, setTheme] = useState(() => {
    return localStorage.getItem("profilerTheme") || "dark";
  });
  const [minWallFilterMs, setMinWallFilterMs] = useState(() => {
    return localStorage.getItem("profilerWallMinFilterMs") || "";
  });
  const [themePreset, setThemePreset] = useState(() => {
    return localStorage.getItem("profilerThemePreset") || "default";
  });
  const [isLoading, setIsLoading] = useState(false);
  const abortControllerRef = useRef<AbortController | null>(null);
  // Persisted TransactionsTab state — survives tab switches (unmount/remount)
  const txTabStateRef = useRef<Record<string, any> | null>(null);
  const [lastLoadedQuery, setLastLoadedQuery] = useState<Record<string, string> | null>(null);
  const setTab = useCallback((tab: string, { replace = false, search = undefined }: { replace?: boolean; search?: string | undefined } = {}) => {
    setActiveTab(tab);
    const url = new URL(window.location.href);
    url.hash = tab === "trace" ? "trace" : "transactions";
    if (tab === "trace" && search != null) {
      url.searchParams.set("search", search);
    } else if (tab !== "trace") {
      url.searchParams.delete("search");
    }
    if (replace) {
      window.history.replaceState({ tab }, "", url);
    } else {
      window.history.pushState({ tab }, "", url);
    }
  }, []);
  const [heatConfig, setHeatConfig] = useState<Record<string, HeatConfigEntry>>(() => {
    const defaults = buildDefaultHeatConfig();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatConfig");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [heatStyleByKey, setHeatStyleByKey] = useState<Record<string, string>>(() => {
    const defaults = buildDefaultHeatStyleMap();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatStyleByKey");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [heatColorByKey, setHeatColorByKey] = useState<Record<string, string>>(() => {
    const defaults = buildDefaultHeatColorMap();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatColorByKey");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [selectedColumns, setSelectedColumns] = useState<string[]>(() => {
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerColumns");
    if (!stored) return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    try {
      const parsed = JSON.parse(stored);
      return sanitizeSelectedColumns(parsed);
    } catch {
      return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    }
  });
  const [numberFormatId, setNumberFormatId] = useState(() => {
    const storage = getSafeStorage();
    return storage?.getItem("profilerNumberFormat") || DEFAULT_NUMBER_FORMAT_ID;
  });
  const [spanVizConfig, setSpanVizConfig] = useState<SpanVizConfig>(() => {
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerSpanVizConfig");
    if (stored) {
      try {
        const parsed = JSON.parse(stored);
        // Migrate old metric keys to new column keys
        if (parsed.metric && !NUMERIC_COLUMN_KEYS.has(parsed.metric)) {
          const metricMigration = {
            wallTotalUs: "wall_inc_total",
            selfWallUs: "wall_self_total",
            busyTotalUs: "busy_inc_total",
            selfBusyUs: "busy_self_total",
            waitTotalUs: "wait_inc_total",
            selfWaitUs: "wait_self_total",
            clarityRuntime: "clarity_runtime_total",
          };
          parsed.metric = (metricMigration as Record<string, string>)[parsed.metric] || "wall_inc_total";
        }
        // Add default color if not present
        if (!parsed.color) {
          parsed.color = DEFAULT_HEAT_COLOR;
        }
        return parsed;
      } catch {
        return { enabled: true, style: "fill", metric: "wall_inc_total", color: DEFAULT_HEAT_COLOR };
      }
    }
    return { enabled: true, style: "fill", metric: "wall_inc_total", color: DEFAULT_HEAT_COLOR };
  });
  const numberFormat = useMemo(() => getNumberFormat(numberFormatId), [numberFormatId]);

  const gridApiRef = useRef<Record<string, any> | null>(null);
  const autoLoadPendingRef = useRef(false);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatConfig", JSON.stringify(heatConfig));
  }, [heatConfig]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerTheme", theme);
  }, [theme]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerWallMinFilterMs", minWallFilterMs);
  }, [minWallFilterMs]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerThemePreset", themePreset);
  }, [themePreset]);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "dark") {
      root.classList.add("dark");
    } else {
      root.classList.remove("dark");
    }
  }, [theme]);

  useEffect(() => {
    const root = document.documentElement;
    if (themePreset === "default") {
      root.removeAttribute("data-theme");
    } else {
      root.setAttribute("data-theme", themePreset);
    }
  }, [themePreset]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => (prev === "dark" ? "light" : "dark"));
  }, []);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerNumberFormat", numberFormatId);
  }, [numberFormatId]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatStyleByKey", JSON.stringify(heatStyleByKey));
  }, [heatStyleByKey]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatColorByKey", JSON.stringify(heatColorByKey));
  }, [heatColorByKey]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerSpanVizConfig", JSON.stringify(spanVizConfig));
  }, [spanVizConfig]);

  // Note: Header menu state is now managed locally within each header cell component
  // (HeatHeaderCell and SpanHeaderCell) to avoid recreating column definitions on every menu open/close

  useEffect(() => {
    getRuns()
      .then((data) => {
        const runsData = data as { id: number; run_name?: string }[];
        setRuns(runsData);
        if (runsData.length > 0) setRunId(String(runsData[0].id));
      })
      .catch((err: Error) => setSummary(err.message));
  }, []);

  useEffect(() => {
    if (!runId) return;
    getBlocks(runId)
      .then((data) => setBlocks(data as { stacks_block_id: string; height: number; block_hash_hex?: string }[]))
      .catch(() => setBlocks([]));
  }, [runId]);

  useEffect(() => {
    const initialTab = window.location.hash === "#trace" ? "trace" : "transactions";
    setActiveTab(initialTab);

    // Parse ?search= query parameter for the trace view
    const params = new URLSearchParams(window.location.search);
    const searchParam = params.get("search");
    if (searchParam && initialTab === "trace") {
      setTxQuery(searchParam);
      // If the search param looks like a valid 64-char hex hash, auto-load once resolved
      if (/^[0-9a-fA-F]{64}$/.test(searchParam.trim())) {
        autoLoadPendingRef.current = true;
      }
    }

    window.history.replaceState({ tab: initialTab }, "", window.location.href);

    const handlePopState = (event: PopStateEvent) => {
      const tab = event.state?.tab || (window.location.hash === "#trace" ? "trace" : "transactions");
      setActiveTab(tab);
      // Restore search param from URL
      const urlParams = new URLSearchParams(window.location.search);
      const searchVal = urlParams.get("search");
      if (tab === "trace" && searchVal) {
        setTxQuery(searchVal);
      }
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    if (!runId) return;
    const normalized = txQuery.trim().toLowerCase();
    if (normalized.length !== 64) return;
    lookupTx(runId, normalized)
      .then((data) => {
        const result = data as { stacks_tx_id?: number };
        if (result?.stacks_tx_id) {
          setStacksTxId(String(result.stacks_tx_id));
        } else {
          setSummary("Tx hash not found in this run.");
        }
      })
      .catch((err: Error) => setSummary(err.message));
  }, [runId, txQuery]);

  const loadTrace = useCallback(async () => {
    if (!runId) return;
    
    // Switch to trace tab when loading trace, include search param if in tx mode
    setTab("trace", { search: mode === "tx" && txQuery ? txQuery : undefined });
    
    // Cancel any existing request
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }
    
    const controller = new AbortController();
    abortControllerRef.current = controller;
    
    const params = {
      run_id: runId,
      mode,
      limit: limit || "5000",
      stacks_tx_id: mode === "tx" && stacksTxId ? stacksTxId : undefined,
      stacks_block_id: mode === "run" && stacksBlockId ? stacksBlockId : undefined,
      segment_root_id: mode === "run" && segmentRootId ? segmentRootId : undefined,
      min_wall_ms: minWallMs || undefined,
    };

    // Clear data immediately when starting a new search
    setRows([]);
    setSummary("Loading trace...");
    setIsLoading(true);
    try {
      const data = await getTrace(params, { signal: controller.signal }) as Record<string, any>[];
      setRows(data);
      if (data.length === 0) {
        setSummary("No data found.");
      } else {
        setSummary(`${data.length} records loaded.`);
      }
      setLastLoadedQuery({
        runId,
        mode,
        txQuery,
        stacksTxId,
        stacksBlockId,
        segmentRootId,
        minWallMs,
        limit,
        hotPathMode,
      });
    } catch (err) {
      if ((err as Error).name === "AbortError") {
        setSummary("Request cancelled.");
      } else {
        setSummary(`Error: ${(err as Error).message}`);
      }
    } finally {
      setIsLoading(false);
      abortControllerRef.current = null;
    }
  }, [runId, mode, limit, stacksTxId, stacksBlockId, segmentRootId, minWallMs, txQuery, hotPathMode, setTab]);

  // Auto-load trace when a valid ?search= param was present on page load
  // and the tx lookup has resolved a stacksTxId.
  useEffect(() => {
    if (autoLoadPendingRef.current && stacksTxId) {
      autoLoadPendingRef.current = false;
      loadTrace();
    }
  }, [stacksTxId, loadTrace]);

  const cancelLoad = useCallback(() => {
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }
  }, []);

  const isDirty = useMemo(() => {
    if (!lastLoadedQuery) return true;
    return (
      lastLoadedQuery.runId !== runId ||
      lastLoadedQuery.mode !== mode ||
      lastLoadedQuery.txQuery !== txQuery ||
      lastLoadedQuery.stacksTxId !== stacksTxId ||
      lastLoadedQuery.stacksBlockId !== stacksBlockId ||
      lastLoadedQuery.segmentRootId !== segmentRootId ||
      lastLoadedQuery.minWallMs !== minWallMs ||
      lastLoadedQuery.limit !== limit ||
      lastLoadedQuery.hotPathMode !== hotPathMode
    );
  }, [lastLoadedQuery, runId, mode, txQuery, stacksTxId, stacksBlockId, segmentRootId, minWallMs, limit, hotPathMode]);

  const resetQuery = useCallback(() => {
    setMode("tx");
    setTxQuery("");
    setStacksTxId("");
    setStacksBlockId("");
    setSegmentRootId("");
    setMinWallMs("");
    setMinWallFilterMs("");
    setLimit("5000");
    setHotPathMode("off");
    setChainCompression(true);
  }, []);

  // Navigate to trace view for a specific transaction
  const viewTransactionTrace = useCallback((txHashHex: string, stacksTxIdValue: number) => {
    setTxQuery(txHashHex);
    setStacksTxId(String(stacksTxIdValue));
    setTab("trace", { search: txHashHex });
    // Trigger load after switching to trace tab
    setTimeout(() => {
      const params = {
        run_id: runId,
        mode: "tx",
        stacks_tx_id: String(stacksTxIdValue),
        limit: limit || "5000",
      };

      setRows([]);
      setSummary("Loading trace...");
      setIsLoading(true);

      getTrace(params)
        .then((result) => {
          const data = result as Record<string, any>[];
          setRows(data);
          setSummary(data.length === 0 ? "No data found." : `${data.length} records loaded.`);
          setLastLoadedQuery({
            runId,
            mode: "tx",
            txQuery: txHashHex,
            stacksTxId: String(stacksTxIdValue),
            stacksBlockId: "",
            segmentRootId: "",
            minWallMs: "",
            limit,
            hotPathMode,
          });
        })
        .catch((err: Error) => {
          setSummary(`Error: ${err.message}`);
        })
        .finally(() => {
          setIsLoading(false);
        });
    }, 50);
  }, [runId, limit, hotPathMode, setTab]);

  const maxWallUs = useMemo(() => {
    return rows.reduce((maxVal, row) => {
      const wall = getWallUs(row);
      if (wall == null) return maxVal;
      return Math.max(maxVal, wall);
    }, 0);
  }, [rows]);

  const baseRows = useMemo(() => {
    return rows.map((row) => ({
      ...row,
      flame_percent: maxWallUs > 0 ? ((getWallUs(row) ?? 0) / maxWallUs) * 100 : 0,
    })) as unknown as TreeNode[];
  }, [rows, maxWallUs]);

  const baseTree = useMemo(() => buildTreeIndex(baseRows), [baseRows]);

  // Compute dynamic filter fields from loaded data
  const traceFilterFields: FilterFieldDef[] = useMemo(() => {
    // Collect distinct spans (name + context + id) and contexts
    const spanMap = new Map<number, { name: string; context: string | null }>();
    const contextSet = new Set<string>();
    const dispatchNames = new Set<string>();
    const builtinNames = new Set<string>();
    // user fn: tag → { name, visibilities }
    const userFnMap = new Map<string, { name: string; visSet: Set<string> }>();

    for (const row of rows) {
      if (row.profiler_span_id != null && row.span_name) {
        spanMap.set(row.profiler_span_id, {
          name: row.span_name,
          context: row.span_context ?? null,
        });
      }
      if (row.span_context) {
        contextSet.add(row.span_context);
        if (row.span_context === "clarity::dispatch" && row.span_name) {
          dispatchNames.add(row.span_name);
        }
        if (row.span_context === "clarity::builtin" && row.span_name) {
          builtinNames.add(row.span_name);
        }
        if (row.span_context.startsWith("clarity::user::") && row.span_name) {
          const vis = row.span_context.slice("clarity::user::".length);
          const label = vis === "read-only" || vis === "read_only" ? "read-only" : vis;
          const key = row.tag ?? row.span_name; // tag uniquely identifies fn per contract
          let entry = userFnMap.get(key);
          if (!entry) { entry = { name: row.span_name, visSet: new Set() }; userFnMap.set(key, entry); }
          entry.visSet.add(label);
        }
      }
    }

    // Build rich options for span name — exclude pseudo-context spans, use profiler_span_id as value
    const spanRichOptions: RichOption[] = Array.from(spanMap.entries())
      .filter(([, s]) => !isClarityPseudoContext(s.context))
      .map(([id, s]) => ({
        value: String(id),
        label: s.name,
        description: s.context ?? undefined,
      }))
      .sort((a, b) => a.label.localeCompare(b.label));

    // Build a profiler_span_id → display label map for chip labels
    const spanIdToLabel = new Map<string, string>();
    for (const [id, s] of spanMap) {
      spanIdToLabel.set(String(id), s.name);
    }

    // Build context enum values — exclude pseudo-contexts (sorted)
    const contextValues = Array.from(contextSet)
      .filter((c) => !isClarityPseudoContext(c))
      .sort();

    // Build dispatch type enum values (sorted)
    const dispatchValues = Array.from(dispatchNames).sort();

    // Build builtin function enum values (sorted)
    const builtinValues = Array.from(builtinNames).sort();

    // Build user function rich options keyed by tag for contract disambiguation
    const userFnRichOptions: RichOption[] = Array.from(userFnMap.entries())
      .map(([tag, { name, visSet }]) => {
        const vis = Array.from(visSet).sort().join(", ");
        // If tag differs from name, show "visibility — contract.fn" for disambiguation
        const desc = tag !== name ? `${vis} — ${tag}` : vis;
        return { value: tag, label: name, description: desc };
      })
      .sort((a, b) => a.label.localeCompare(b.label));

    // Map tag → display name for chip labels
    const userFnTagToLabel = new Map<string, string>();
    for (const [tag, { name }] of userFnMap) {
      userFnTagToLabel.set(tag, name);
    }

    // Dynamic fields
    const dynamicFields: FilterFieldDef[] = [
      {
        id: "span_name",
        label: "Span Name",
        type: "text",
        operators: SPAN_NAME_OPERATORS,
        richOptions: spanRichOptions.length > 0 ? spanRichOptions : undefined,
        chipLabel: (v: string) => spanIdToLabel.get(v) ?? v,
      },
      {
        id: "span_context",
        label: "Context",
        type: "text",
        operators: CONTEXT_OPERATORS,
        enumValues: contextValues.length > 0 ? contextValues : undefined,
      },
    ];

    // Clarity-specific filters (only if data exists)
    const clarityFields: FilterFieldDef[] = [];
    if (builtinValues.length > 0) {
      clarityFields.push({
        id: "clarity_builtin_fn",
        label: "Built-in Function",
        type: "enum",
        enumValues: builtinValues,
        group: "Clarity",
      });
    }
    if (userFnRichOptions.length > 0) {
      clarityFields.push({
        id: "clarity_user_fn",
        label: "User Function",
        type: "enum",
        richOptions: userFnRichOptions,
        chipLabel: (v: string) => userFnTagToLabel.get(v) ?? v,
        group: "Clarity",
      });
    }
    if (dispatchValues.length > 0) {
      clarityFields.push({
        id: "clarity_dispatch_type",
        label: "Dispatch Type",
        type: "enum",
        enumValues: dispatchValues,
        group: "Clarity",
      });
    }

    return [...dynamicFields, ...clarityFields, ...TRACE_FILTER_FIELDS_STATIC];
  }, [rows]);

  // Client-side autocomplete for trace filter text fields
  const traceFilterOptions = useCallback(
    async (fieldId: string, query: string, _signal: AbortSignal): Promise<string[]> => {
      if (!query) return [];
      const q = query.toLowerCase();
      const seen = new Set<string>();
      const results: string[] = [];
      for (const row of rows) {
        const val =
          fieldId === "span_name" ? row.span_name :
          fieldId === "span_context" ? row.span_context :
          fieldId === "tag" ? row.tag :
          null;
        if (val && !seen.has(val) && val.toLowerCase().includes(q)) {
          seen.add(val);
          results.push(val);
          if (results.length >= 50) break;
        }
      }
      return results.sort();
    },
    [rows]
  );

  const minWallFilterUs = useMemo(
    () => (minWallFilterMs ? Number(minWallFilterMs) * 1000 : null),
    [minWallFilterMs]
  );

  const prunedRoots = useMemo(
    () => pruneTree(baseTree.roots, minWallFilterUs),
    [baseTree.roots, minWallFilterUs]
  );

  const prunedById = useMemo(() => indexTree(prunedRoots), [prunedRoots]);

  const focusedTree = useMemo(
    () => applyFocus(prunedRoots, prunedById, focusId),
    [prunedRoots, prunedById, focusId]
  );

  const hotPathRoots = useMemo(
    () => applyHotPath(focusedTree.roots, hotPathMode),
    [focusedTree.roots, hotPathMode]
  );

  const { roots: filteredRoots, totalFiltered: traceFilteredCount } = useMemo(
    () => traceFilterEnabled ? applyTreeFilter(hotPathRoots, traceFilter) : { roots: hotPathRoots, totalFiltered: 0 },
    [hotPathRoots, traceFilter, traceFilterEnabled]
  );

  const compressedRoots = useMemo(
    () =>
      applyChainCompression(filteredRoots, {
        enabled: chainCompression,
        expandedChains,
        significantSelfUs: DEFAULT_AUTO_EXPAND.selfMs * 1000,
      }),
    [filteredRoots, chainCompression, expandedChains]
  );

  const withFlamePercent = useMemo(() => {
    const applyPercent = (node: TreeNode): TreeNode => {
      const wall = getWallUs(node) ?? 0;
      const flame_percent = maxWallUs > 0 ? (wall / maxWallUs) * 100 : 0;
      return {
        ...node,
        flame_percent,
        data: (node.data || []).map(applyPercent),
      };
    };
    return compressedRoots.map(applyPercent);
  }, [compressedRoots, maxWallUs]);

  const roots = useMemo(
    () => applyOpenState(withFlamePercent, openNodes, hotPathMode !== "off"),
    [withFlamePercent, openNodes, hotPathMode]
  );

  const toggleHeat = useCallback((heatKey: string) => {
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        enabled: !(prev[heatKey]?.enabled ?? false),
      },
    }));
  }, []);

  const setHeatMin = useCallback((heatKey: string, value: string | number | null) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        min: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const setHeatMax = useCallback((heatKey: string, value: string | number | null) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        max: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const setHeatStyleForKey = useCallback((heatKey: string, value: string) => {
    setHeatStyleByKey((prev) => ({
      ...prev,
      [heatKey]: value,
    }));
  }, []);

  const setHeatColorForKey = useCallback((heatKey: string, value: string) => {
    setHeatColorByKey((prev) => ({
      ...prev,
      [heatKey]: value,
    }));
  }, []);

  // Get raw numeric value for a column from a row (using column's getter)
  const getColumnValue = useCallback((colKey: string, row: Record<string, any>): number | null => {
    const col = COLUMN_BY_KEY[colKey];
    if (!col) return null;
    const value = col.getter ? col.getter(row) : row[col.key];
    if (typeof value === "number" && Number.isFinite(value)) return value;
    if (typeof value === "string") {
      const parsed = Number(value);
      return Number.isFinite(parsed) ? parsed : null;
    }
    return null;
  }, []);

  // Compute max values for all heat-capable columns
  const heatMax = useMemo(() => {
    const flat = flattenTree(withFlamePercent);
    const initial: Record<string, number> = {};
    for (const col of HEAT_CAPABLE_COLUMNS) {
      initial[col.key] = 0;
    }
    return flat.reduce((acc, row) => {
      for (const col of HEAT_CAPABLE_COLUMNS) {
        const raw = getColumnValue(col.key, row);
        if (raw != null && Number.isFinite(raw)) {
          acc[col.key] = Math.max(acc[col.key] ?? 0, raw);
        }
      }
      return acc;
    }, initial);
  }, [withFlamePercent, getColumnValue]);

  const getHeatBounds = useCallback(
    (colKey: string) => {
      const config = heatConfig[colKey] || {};
      const min = config.min ?? 0;
      const max = config.max ?? heatMax[colKey] ?? 0;
      return { min, max, enabled: config.enabled === true };
    },
    [heatConfig, heatMax]
  );

  // Compute span viz value (level 0-1 and pct 0-100) for a row
  const getSpanVizValue = useCallback(
    (row: Record<string, any>) => {
      const metric = spanVizConfig.metric || "wall_inc_total";
      const raw = getColumnValue(metric, row);
      const max = heatMax[metric] ?? 0;
      const min = 0; // Always use 0 as min for span viz (auto scaling)

      if (raw == null || raw <= 0 || max <= min) {
        return { level: 0, pct: 0 };
      }

      const pct = Math.min(100, Math.max(0, ((raw - min) / (max - min)) * 100));
      const level = pct / 100;
      return { level, pct };
    },
    [spanVizConfig.metric, heatMax, getColumnValue]
  );

  const breadcrumb = focusedTree.breadcrumb ?? [];

  // Flat list of all rows for navigation in span details modal
  const allRowsFlat = useMemo(() => flattenTree(withFlamePercent), [withFlamePercent]);

  // Get the currently active span for the details modal
  const activeSpan = useMemo(() => {
    if (activeId == null) return null;
    return allRowsFlat.find((row) => row.id === activeId) || null;
  }, [activeId, allRowsFlat]);

  // Get the index of the active span in the flat list
  const activeSpanIndex = useMemo(() => {
    if (activeId == null) return -1;
    return allRowsFlat.findIndex((row) => row.id === activeId);
  }, [activeId, allRowsFlat]);

  // Navigation for span details modal
  const hasPreviousSpan = activeSpanIndex > 0;
  const hasNextSpan = activeSpanIndex >= 0 && activeSpanIndex < allRowsFlat.length - 1;

  const navigateToPreviousSpan = useCallback(() => {
    if (activeSpanIndex > 0) {
      setActiveId(allRowsFlat[activeSpanIndex - 1].id);
    }
  }, [activeSpanIndex, allRowsFlat]);

  const navigateToNextSpan = useCallback(() => {
    if (activeSpanIndex >= 0 && activeSpanIndex < allRowsFlat.length - 1) {
      setActiveId(allRowsFlat[activeSpanIndex + 1].id);
    }
  }, [activeSpanIndex, allRowsFlat]);

  useEffect(() => {
    setOpenNodes(computeDefaultOpenSet(focusedTree.roots, DEFAULT_AUTO_EXPAND));
  }, [rows, focusId, minWallFilterMs, focusedTree.roots]);

  const toggleChain = useCallback((rowId: string | number) => {
    setExpandedChains((prev) => {
      const next = new Set(prev);
      if (next.has(rowId)) {
        next.delete(rowId);
      } else {
        next.add(rowId);
      }
      return next;
    });
  }, []);

  /** Expand a compressed chain down to a specific segment index.
   *  Adds the IDs of all segments from 0..segmentIndex-1 to expandedChains
   *  so the chain re-compresses into two pieces: before the clicked segment
   *  (now expanded) and after (still compressed). */
  const expandChainTo = useCallback((segments: Array<{ id?: string | number }>, segmentIndex: number) => {
    if (!Array.isArray(segments) || segmentIndex <= 0) return;
    setExpandedChains((prev) => {
      const next = new Set(prev);
      // Mark each node up to (but not including) the target segment as expanded
      for (let i = 0; i < segmentIndex; i++) {
        if (segments[i]?.id != null) next.add(segments[i].id!);
      }
      return next;
    });
    // Ensure the tree is open down to the target so it's visible
    setOpenNodes((prev) => {
      const next = new Set(prev);
      for (let i = 0; i < segmentIndex; i++) {
        if (segments[i]?.id != null) next.add(segments[i].id!);
      }
      return next;
    });
    // Select the target segment (the one the user clicked / expanded to).
    // We need to update both activeId (App-level selection for details panel)
    // and the SVAR Grid's visual row selection via gridApi.
    // The grid selection is deferred with requestAnimationFrame so it runs
    // after React re-renders the tree with the newly expanded rows.
    const targetId = segments[segmentIndex - 1]?.id;
    if (targetId != null) {
      setActiveId(targetId);
      requestAnimationFrame(() => {
        gridApiRef.current?.exec("select-row", { id: targetId });
      });
    }
  }, []);

  const focusNode = useCallback((rowId: string | number) => {
    setFocusId(rowId);
    setActiveId(rowId);
  }, []);

  const clearFocus = useCallback(() => {
    setFocusId(null);
  }, []);

  const expandToDepth = useCallback(
    (depth: number) => {
      const openIds = new Set<string | number>();
      const walk = (node: Record<string, any>, currentDepth: number) => {
        if (currentDepth < depth) openIds.add(node.id);
        if (currentDepth < depth) {
          (node.data || []).forEach((child: TreeNode) => walk(child, currentDepth + 1));
        }
      };
      focusedTree.roots.forEach((node) => walk(node, 0));
      setOpenNodes(openIds);
    },
    [focusedTree.roots]
  );

  const collapseSiblings = useCallback(() => {
    if (!activeId) return;
    const activeNode = prunedById.get(activeId);
    if (!activeNode || !activeNode.parent_id) return;
    const parent = prunedById.get(activeNode.parent_id);
    if (!parent) return;
    const next = new Set(openNodes);
    (parent.data ?? []).forEach((child: TreeNode) => {
      if (child.id !== activeId) {
        const ids = new Set<string | number>();
        collectSubtreeIds(child, ids);
        ids.forEach((id) => next.delete(id));
      }
    });
    next.add(parent.id);
    next.add(activeId);
    setOpenNodes(next);
  }, [activeId, prunedById, openNodes]);

  // Build a static header function that uses context-aware components.
  // This function produces stable header objects because the actual configuration
  // is fetched from context at render time, not embedded in the column definition.
  const buildHeatHeader = useCallback((col: { level3?: string; headerLabel?: string; label: string }) => ({
    text: col.level3 || col.headerLabel || col.label,
    cell: ContextHeatHeaderCell,
  }), []);

  const buildSpanHeader = useCallback((col: { label: string }) => ({
    text: col.label,
    cell: ContextSpanHeaderCell,
  }), []);

  const visibleColumns = useMemo(() => {
    // Build 3-element header arrays for all columns
    // [level1Header, level2Header, level3Header]
    const build3LevelHeader = (col: ColumnDef) => {
      // Level 1: Top group (Wall Time, Clarity, etc.) or empty for standalone
      const level1 = col.level1Start
        ? { text: col.level1, colspan: col.level1Span, css: "grid-group-header grid-level1-header" }
        : col.level1
          ? { text: "", _hidden: true }
          : { text: "", css: "grid-level1-empty" };
      
      // Level 2: Subgroup (Inclusive, Self, Runtime, etc.) or empty for standalone
      const level2 = col.level2Start
        ? { text: col.level2, colspan: col.level2Span, css: "grid-group-header grid-level2-header" }
        : col.level2
          ? { text: "", _hidden: true }
          : { text: "", css: "grid-level2-empty" };
      
      // Level 3: Column label (Total, Avg, or standalone label)
      // All numeric columns get the heat header (with settings popover)
      const isNumeric = NUMERIC_COLUMN_KEYS.has(col.key);
      const level3 = col.key === "span"
        ? buildSpanHeader(col)
        : isNumeric
          ? buildHeatHeader(col)
          : { text: col.level3 || col.label };
      
      return [level1, level2, level3];
    };

    return COLUMN_DEFS.map((col) => {
      const isNumeric = NUMERIC_COLUMN_KEYS.has(col.key);
      const baseWidth = col.width ?? (isNumeric ? DEFAULT_NUMERIC_WIDTH : undefined);
      const width = isNumeric && baseWidth != null
        ? Math.max(MIN_NUMERIC_WIDTH, baseWidth)
        : baseWidth;
      
      // Determine the cell renderer - using context-aware components that
      // fetch their configuration from ProfilerGridContext at render time.
      // This allows the column definition to remain stable.
      let cellRenderer;
      if (col.key === "span") {
        // SpanCell gets its callbacks from context
        cellRenderer = SpanCell;
      } else if (isNumeric) {
        // ContextHeatCell gets heat configuration from context
        cellRenderer = ContextHeatCell;
      } else {
        cellRenderer = col.cell;
      }
      
      return {
        id: col.key,
        header: build3LevelHeader(col),
        width,
        flexgrow: col.flexgrow,
        treetoggle: col.treetoggle,
        // Store the original column definition for context-aware cells to access
        _colDef: col,
        cell: cellRenderer,
        getter: col.getter,
        hidden: col.alwaysHidden
          ? true
          : col.alwaysVisible
            ? false
            : selectedColumns.includes(col.key)
              ? false
              : true,
        resize: true,
      };
    });
  }, [
    selectedColumns,
    // buildHeatHeader and buildSpanHeader are now useCallback with no deps,
    // so they're stable and don't need to be in the dependency array
  ]);

  const toggleColumn = (key: string) => {
    setSelectedColumns((prev) => {
      if (ALWAYS_VISIBLE_KEYS.has(key) || ALWAYS_HIDDEN_KEYS.has(key)) return prev;
      const columnDef = COLUMN_DEFS.find((col) => col.key === key);
      if (!columnDef || columnDef.selectable === false) return prev;
      const next = prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key];
      const sanitized = sanitizeSelectedColumns(next);
      localStorage.setItem("profilerColumns", JSON.stringify(sanitized));
      return sanitized;
    });
  };

  const toggleColumnGroup = (keys: string[], enable: boolean) => {
    setSelectedColumns((prev) => {
      let next = [...prev];
      for (const key of keys) {
        if (ALWAYS_VISIBLE_KEYS.has(key) || ALWAYS_HIDDEN_KEYS.has(key)) continue;
        const columnDef = COLUMN_DEFS.find((col) => col.key === key);
        if (!columnDef || columnDef.selectable === false) continue;
        if (enable && !next.includes(key)) {
          next.push(key);
        } else if (!enable && next.includes(key)) {
          next = next.filter((k) => k !== key);
        }
      }
      const sanitized = sanitizeSelectedColumns(next);
      localStorage.setItem("profilerColumns", JSON.stringify(sanitized));
      return sanitized;
    });
  };

  // Callbacks for the grid context - these are accessed via context to avoid
  // recreating the visibleColumns array when callbacks change.
  // We use a ref pattern here: the context holds stable wrapper functions
  // that always call the latest version of these callbacks.
  const profilerGridCallbacks = {
    // SpanCell callbacks
    toggleChain,
    expandChainTo,
    focusNode,
    spanVizConfig,
    getSpanVizValue,
    // HeatCell callbacks  
    getHeatBounds,
    getColumnValue,
    heatColorByKey,
    heatStyleByKey,
    numberFormat,
    // Heat header configuration
    heatConfig,
    toggleHeat,
    setHeatMin,
    setHeatMax,
    setHeatStyleForKey,
    setHeatColorForKey,
    heatColorOptions: HEAT_COLOR_OPTIONS,
    minWallFilterMs,
    setMinWallFilterMs,
    defaultHeatStyle: DEFAULT_HEAT_STYLE,
    defaultHeatColor: DEFAULT_HEAT_COLOR,
    // Span header configuration
    setSpanVizConfig,
  };

  return (
    <div className={`app-container ${theme}`}>
      {/* Settings Panel */}
      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        mode={mode}
        setMode={setMode}
        minWallMs={minWallMs}
        setMinWallMs={setMinWallMs}
        limit={limit}
        setLimit={setLimit}
        hotPathMode={hotPathMode}
        setHotPathMode={setHotPathMode as (mode: string) => void}
        chainCompression={chainCompression}
        setChainCompression={setChainCompression}
        numberFormatId={numberFormatId}
        setNumberFormatId={setNumberFormatId}
        themePreset={themePreset}
        setThemePreset={setThemePreset}
        segmentRootId={segmentRootId}
        setSegmentRootId={setSegmentRootId}
        stacksBlockId={stacksBlockId}
        setStacksBlockId={setStacksBlockId}
        blocks={blocks}
        numberFormats={NUMBER_FORMATS}
        themePresets={THEME_PRESETS}
      />

      {/* Header */}
      <HeaderBar
        runs={runs}
        runId={runId}
        setRunId={setRunId}
        txQuery={txQuery}
        setTxQuery={setTxQuery}
        mode={mode}
        rowsLength={rows.length}
        theme={theme}
        toggleTheme={toggleTheme}
        resetQuery={resetQuery}
        onOpenSettings={() => setSettingsOpen(true)}
        loadTrace={loadTrace}
        cancelLoad={cancelLoad}
        isDirty={isDirty}
        isLoading={isLoading}
        activeTab={activeTab}
        setActiveTab={setTab}
      />

      {/* Trace View */}
      {activeTab === "trace" && (
        <>
          {/* Toolbar */}
          <ToolbarBar
            selectedColumns={selectedColumns}
        toggleColumn={toggleColumn}
        toggleColumnGroup={toggleColumnGroup}
        expandToDepth={expandToDepth}
        hotPathMode={hotPathMode}
        setHotPathMode={setHotPathMode as (mode: string) => void}
        chainCompression={chainCompression}
        setChainCompression={setChainCompression}
        collapseSiblings={collapseSiblings}
        activeId={activeId}
        focusId={focusId}
        clearFocus={clearFocus}
        summary={summary}
      />

          {/* Trace Filter */}
          <div className="trace-filter-bar">
            <FilterBuilder
              fields={traceFilterFields}
              value={traceFilter as FilterGroupValue}
              onChange={(v) => setTraceFilter(v as TreeFilterGroup)}
              options={traceFilterOptions}
              filtersEnabled={traceFilterEnabled}
              onToggleEnabled={() => setTraceFilterEnabled((p) => !p)}
              onClear={() => { setTraceFilter({ glue: "and", rules: [] }); setTraceFilterEnabled(true); }}
            />
            {traceFilteredCount > 0 && (
              <span className="trace-filter-count">
                {traceFilteredCount} span{traceFilteredCount !== 1 ? "s" : ""} filtered
              </span>
            )}
          </div>

          {/* Breadcrumb */}
          <BreadcrumbBar breadcrumb={breadcrumb} />

          {/* Grid */}
          <ProfilerGridProvider callbacks={profilerGridCallbacks}>
            <ProfilerGrid
              data={roots}
              columns={visibleColumns}
              spanVizEnabled={spanVizConfig.enabled}
              spanVizStyle={spanVizConfig.style}
              spanVizColor={spanVizConfig.color ?? DEFAULT_HEAT_COLOR}
              isLoading={isLoading}
              isEmpty={rows.length === 0 && !isLoading}
              rowStyle={() => "profiler-row"}
              columnStyle={(column: any) =>
                NUMERIC_COLUMN_KEYS.has(column.id)
                  ? "grid-col-numeric"
                  : column.id === "span"
                    ? "grid-col-span"
                    : ""
              }
              onInit={(api) => {
                gridApiRef.current = api;
              }}
              onOpenRow={(ev) => {
                setOpenNodes((prev) => new Set(prev).add(ev.id));
              }}
              onCloseRow={(ev) => {
                setOpenNodes((prev) => {
                  const next = new Set(prev);
                  next.delete(ev.id);
                  return next;
                });
              }}
              onSelectRow={(ev) => {
                if (ev?.id != null) {
                  setActiveId(ev.id);
                }
              }}
              // Context menu callbacks
              onViewDetails={(row) => {
                setActiveId(row.id);
                setSpanDetailsOpen(true);
              }}
              onCollapseSiblings={(rowId) => {
                setActiveId(rowId);
                collapseSiblings();
              }}
              onFocus={focusNode}
              onClearFocus={clearFocus}
              onExpandChain={toggleChain}
              focusId={focusId}
            />
          </ProfilerGridProvider>
        </>
      )}

      {/* Transactions View */}
      {activeTab === "transactions" && (
        <TransactionsTab
          runId={runId}
          onViewTrace={viewTransactionTrace}
          numberFormat={numberFormat}
          savedState={txTabStateRef}
        />
      )}

      {/* Span Details Modal */}
      <SpanDetailsModal
        open={spanDetailsOpen}
        onOpenChange={setSpanDetailsOpen}
        span={activeSpan}
        onPrevious={navigateToPreviousSpan}
        onNext={navigateToNextSpan}
        hasPrevious={hasPreviousSpan}
        hasNext={hasNextSpan}
        numberFormat={numberFormat}
      />
    </div>
  );
}
