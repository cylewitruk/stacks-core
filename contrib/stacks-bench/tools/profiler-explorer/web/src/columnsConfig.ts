import { toAvgMs, toMs } from "./treeTransforms.ts";

type ColumnFormat = {
  decimals?: number;
};

/**
 * 3-level column header structure:
 * - level1: Top-level group (e.g., "Wall Time (ms)", "Clarity")
 * - level2: Mid-level subgroup (e.g., "Inclusive", "Self", "Runtime")
 * - level3: Column label (e.g., "Total", "Avg.")
 * 
 * For columns with level1/level2, use colspan to span multiple columns.
 * Columns that start a group/subgroup have `level1Start: true` / `level2Start: true`.
 */
export type ColumnDef = {
  key: string;
  label: string;
  width?: number;
  flexgrow?: number;
  default?: boolean;
  selectable?: boolean;
  alwaysVisible?: boolean;
  alwaysHidden?: boolean;
  treetoggle?: boolean;
  format?: ColumnFormat;
  getter?: (row: Record<string, any>) => any;
  cell?: (props: any) => any;
  heatKey?: string;
  headerLabel?: string;
  // 3-level header structure
  level1?: string;
  level1Span?: number;
  level1Start?: boolean;
  level2?: string;
  level2Span?: number;
  level2Start?: boolean;
  level3?: string;
};

export const COLUMN_DEFS: ColumnDef[] = [
  // ═══════════════════════════════════════════════════════════════════════════
  // Standalone columns (no grouping)
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "span",
    label: "Span",
    level3: "Span",
    width: 360,
    flexgrow: 1,
    default: true,
    selectable: false,
    alwaysVisible: true,
    treetoggle: true,
  },
  {
    key: "calls",
    label: "Calls",
    level3: "Calls",
    width: 80,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.call_count ?? "-",
  },
  {
    key: "tag",
    label: "Tag",
    level3: "Tag",
    width: 180,
    default: false,
    getter: (row) => row.tag ?? "-",
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Wall Time (ms) - 4 columns: Inclusive (Total, Avg), Self (Total, Avg)
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "wall_inc_total",
    label: "Wall Inclusive Total",
    level1: "Wall Time (ms)",
    level1Span: 4,
    level1Start: true,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: true,
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_wall_us ?? row.wall_time_us),
  },
  {
    key: "wall_inc_avg",
    label: "Wall Inclusive Avg",
    level1: "Wall Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: true,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_wall_us ?? row.wall_time_us, row),
  },
  {
    key: "wall_self_total",
    label: "Wall Self Total",
    level1: "Wall Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: true,
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_self_wall_us ?? row.self_wall_time_us),
  },
  {
    key: "wall_self_avg",
    label: "Wall Self Avg",
    level1: "Wall Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: true,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_self_wall_us ?? row.self_wall_time_us, row),
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Busy Time (ms) - 4 columns: Inclusive (Total, Avg), Self (Total, Avg)
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "busy_inc_total",
    label: "Busy Inclusive Total",
    level1: "Busy Time (ms)",
    level1Span: 4,
    level1Start: true,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: false,
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_cpu_us ?? row.cpu_time_us),
  },
  {
    key: "busy_inc_avg",
    label: "Busy Inclusive Avg",
    level1: "Busy Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: false,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_cpu_us ?? row.cpu_time_us, row),
  },
  {
    key: "busy_self_total",
    label: "Busy Self Total",
    level1: "Busy Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: false,
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_self_cpu_us ?? row.self_cpu_time_us),
  },
  {
    key: "busy_self_avg",
    label: "Busy Self Avg",
    level1: "Busy Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: false,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_self_cpu_us ?? row.self_cpu_time_us, row),
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Wait Time (ms) - 4 columns: Inclusive (Total, Avg), Self (Total, Avg)
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "wait_inc_total",
    label: "Wait Inclusive Total",
    level1: "Wait Time (ms)",
    level1Span: 4,
    level1Start: true,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: false,
    format: { decimals: 3 },
    getter: (row) => {
      const wall = row.est_wall_us ?? row.wall_time_us;
      const cpu = row.est_cpu_us ?? row.cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toMs(Math.max(0, wall - cpu));
    },
  },
  {
    key: "wait_inc_avg",
    label: "Wait Inclusive Avg",
    level1: "Wait Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Inclusive",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: false,
    format: { decimals: 3 },
    getter: (row) => {
      const wall = row.est_wall_us ?? row.wall_time_us;
      const cpu = row.est_cpu_us ?? row.cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toAvgMs(Math.max(0, wall - cpu), row);
    },
  },
  {
    key: "wait_self_total",
    label: "Wait Self Total",
    level1: "Wait Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 100,
    default: false,
    format: { decimals: 3 },
    getter: (row) => {
      const wall = row.est_self_wall_us ?? row.self_wall_time_us;
      const cpu = row.est_self_cpu_us ?? row.self_cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toMs(Math.max(0, wall - cpu));
    },
  },
  {
    key: "wait_self_avg",
    label: "Wait Self Avg",
    level1: "Wait Time (ms)",
    level1Span: 4,
    level1Start: false,
    level2: "Self",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 80,
    default: false,
    format: { decimals: 3 },
    getter: (row) => {
      const wall = row.est_self_wall_us ?? row.self_wall_time_us;
      const cpu = row.est_self_cpu_us ?? row.self_cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toAvgMs(Math.max(0, wall - cpu), row);
    },
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Captured K/V Pairs (standalone)
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "kv_total",
    label: "Captured K/V Pairs",
    level3: "Captured K/V",
    width: 100,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.kv_total ?? "-",
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Clarity - 12 columns under one top-level group
  // ═══════════════════════════════════════════════════════════════════════════
  // Runtime
  {
    key: "clarity_runtime_total",
    label: "Clarity Runtime Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: true,
    level2: "Runtime",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 120,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_runtime_total ?? "-",
  },
  {
    key: "clarity_runtime_avg",
    label: "Clarity Runtime Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Runtime",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 120,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_runtime_avg ?? "-",
  },
  // Cost Input (n)
  {
    key: "clarity_input_n_total",
    label: "Clarity Cost Input Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Cost Input (n)",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 120,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_input_n_total ?? "-",
  },
  {
    key: "clarity_input_n_avg",
    label: "Clarity Cost Input Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Cost Input (n)",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 120,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_input_n_avg ?? "-",
  },
  // Read Count
  {
    key: "clarity_read_count_total",
    label: "Clarity Read Count Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Read Count",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_read_count_total ?? "-",
  },
  {
    key: "clarity_read_count_avg",
    label: "Clarity Read Count Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Read Count",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 70,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_read_count_avg ?? "-",
  },
  // Read Length
  {
    key: "clarity_read_length_total",
    label: "Clarity Read Length Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Read Length",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_read_length_total ?? "-",
  },
  {
    key: "clarity_read_length_avg",
    label: "Clarity Read Length Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Read Length",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 70,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_read_length_avg ?? "-",
  },
  // Write Count
  {
    key: "clarity_write_count_total",
    label: "Clarity Write Count Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Write Count",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_write_count_total ?? "-",
  },
  {
    key: "clarity_write_count_avg",
    label: "Clarity Write Count Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Write Count",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 70,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_write_count_avg ?? "-",
  },
  // Write Length
  {
    key: "clarity_write_length_total",
    label: "Clarity Write Length Total",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Write Length",
    level2Span: 2,
    level2Start: true,
    level3: "Total",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.clarity_write_length_total ?? "-",
  },
  {
    key: "clarity_write_length_avg",
    label: "Clarity Write Length Avg",
    level1: "Clarity",
    level1Span: 12,
    level1Start: false,
    level2: "Write Length",
    level2Span: 2,
    level2Start: false,
    level3: "Avg.",
    width: 70,
    default: true,
    format: { decimals: 2 },
    getter: (row) => row.clarity_write_length_avg ?? "-",
  },
  // ═══════════════════════════════════════════════════════════════════════════
  // Hidden columns
  // ═══════════════════════════════════════════════════════════════════════════
  {
    key: "samples",
    label: "Samples",
    level3: "Samples",
    width: 80,
    default: false,
    format: { decimals: 0 },
    getter: (row) => row.sample_count ?? "-",
  },
  {
    key: "record_id",
    label: "Record ID",
    level3: "Record ID",
    width: 120,
    default: false,
    selectable: false,
    alwaysHidden: true,
    format: { decimals: 0 },
    getter: (row) => row.id,
  },
];

export const NUMERIC_COLUMN_KEYS = new Set([
  "calls",
  "samples",
  "kv_total",
  "wall_inc_total",
  "wall_inc_avg",
  "wall_self_total",
  "wall_self_avg",
  "busy_inc_total",
  "busy_inc_avg",
  "busy_self_total",
  "busy_self_avg",
  "wait_inc_total",
  "wait_inc_avg",
  "wait_self_total",
  "wait_self_avg",
  "clarity_runtime_total",
  "clarity_runtime_avg",
  "clarity_input_n_total",
  "clarity_input_n_avg",
  "clarity_read_count_total",
  "clarity_read_count_avg",
  "clarity_read_length_total",
  "clarity_read_length_avg",
  "clarity_write_count_total",
  "clarity_write_count_avg",
  "clarity_write_length_total",
  "clarity_write_length_avg",
  "record_id",
]);

export const COUNT_DIM_KEYS = new Set([
  "clarity_runtime_total",
  "clarity_runtime_avg",
  "clarity_input_n_total",
  "clarity_input_n_avg",
  "clarity_read_count_total",
  "clarity_read_count_avg",
  "clarity_read_length_total",
  "clarity_read_length_avg",
  "clarity_write_count_total",
  "clarity_write_count_avg",
  "clarity_write_length_total",
  "clarity_write_length_avg",
]);

export const ALWAYS_VISIBLE_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysVisible).map((c) => c.key)
);

export const ALWAYS_HIDDEN_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysHidden).map((c) => c.key)
);

export const SELECTABLE_COLUMNS = COLUMN_DEFS.filter((c) => c.selectable !== false);

export const DEFAULT_COLUMNS = COLUMN_DEFS.filter((c) => c.default)
  .map((c) => c.key)
  .filter((key) => !ALWAYS_HIDDEN_KEYS.has(key));

export const sanitizeSelectedColumns = (values: unknown) => {
  const base = Array.isArray(values)
    ? values.filter((key) => !ALWAYS_HIDDEN_KEYS.has(String(key)))
    : [];
  ALWAYS_VISIBLE_KEYS.forEach((key) => {
    if (!base.includes(key)) base.push(key);
  });
  return base.length > 0 ? base : DEFAULT_COLUMNS;
};

/**
 * Build hierarchical column groups for the column selector.
 * Returns a structure like:
 * [
 *   { key: "calls", label: "Calls", type: "column" },
 *   { 
 *     key: "Wall Time (ms)", label: "Wall Time (ms)", type: "group",
 *     children: [
 *       { key: "Inclusive", label: "Inclusive", type: "subgroup", columns: [...] },
 *       { key: "Self", label: "Self", type: "subgroup", columns: [...] },
 *     ]
 *   },
 *   ...
 * ]
 */
export function buildColumnHierarchy() {
  const result: any[] = [];
  const level1Map = new Map<string, any>();
  
  for (const col of COLUMN_DEFS) {
    if (col.alwaysHidden || col.selectable === false) continue;
    
    if (!col.level1) {
      // Standalone column
      result.push({ key: col.key, label: col.label, type: "column" });
    } else {
      // Grouped column
      if (!level1Map.has(col.level1)) {
        const group = {
          key: col.level1,
          label: col.level1,
          type: "group",
          children: new Map<string, any>(),
          columnKeys: [] as string[],
        };
        level1Map.set(col.level1, group);
        result.push(group);
      }
      
      const group = level1Map.get(col.level1)!;
      group.columnKeys.push(col.key);
      
      if (col.level2) {
        if (!group.children.has(col.level2)) {
          group.children.set(col.level2, {
            key: `${col.level1}::${col.level2}`,
            label: col.level2,
            type: "subgroup",
            columns: [],
            columnKeys: [] as string[],
          });
        }
        const subgroup = group.children.get(col.level2)!;
        subgroup.columns.push({ key: col.key, label: col.level3 || col.label });
        subgroup.columnKeys.push(col.key);
      }
    }
  }
  
  // Convert children Maps to arrays
  for (const item of result) {
    if (item.type === "group" && item.children instanceof Map) {
      item.children = Array.from(item.children.values());
    }
  }
  
  return result;
}
