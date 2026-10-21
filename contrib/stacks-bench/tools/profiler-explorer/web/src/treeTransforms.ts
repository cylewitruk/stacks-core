export type TreeNode = Record<string, any> & {
  id: string | number;
  parent_id?: string | number | null;
  data?: TreeNode[];
  sort_path?: string | null;
  _filtered_children?: number;
};

// ---------------------------------------------------------------------------
// Tree filter types
// ---------------------------------------------------------------------------

export type TreeFilterOperator =
  | "contains" | "notContains"
  | "equal" | "notEqual"
  | "beginsWith" | "endsWith"
  | "greater" | "greaterOrEqual"
  | "less" | "lessOrEqual"
  | "is" | "isNot";

export interface TreeFilterRule {
  field: string;
  operator: TreeFilterOperator;
  value?: string | number | null;
  values?: string[];
  /** Duration modifier: "ms" | "us" | "s".  Accessors return ms. */
  modifier?: string;
}

export interface TreeFilterGroup {
  glue: "and" | "or";
  rules: (TreeFilterRule | TreeFilterGroup)[];
}

type DefaultAutoExpand = {
  depth: number;
  selfMs: number;
  wallMs: number;
  topKChildren: number;
};

type ChainCompressionOptions = {
  enabled: boolean;
  expandedChains: Set<string | number>;
  significantSelfUs?: number | null;
};

export function toMs(value: number | null | undefined): number | null {
  if (value === null || value === undefined) return null;
  return value / 1000;
}

export function toAvgMs(totalUs: number | null | undefined, row: { call_count?: number } | null | undefined): number | null {
  if (totalUs === null || totalUs === undefined) return null;
  const calls = row?.call_count ?? 0;
  if (!Number.isFinite(calls) || calls <= 0) return null;
  return (totalUs / calls) / 1000;
}

export function getWallUs(row: Record<string, any>): number | null {
  return row.est_wall_us ?? row.wall_time_us ?? null;
}

export function getSelfWallUs(row: Record<string, any>): number | null {
  return row.est_self_wall_us ?? row.self_wall_time_us ?? null;
}

export function getSelfCpuUs(row: Record<string, any>): number | null {
  return row.est_self_cpu_us ?? row.self_cpu_time_us ?? null;
}

export function getSelfWaitUs(row: Record<string, any>): number | null {
  const wall = getSelfWallUs(row);
  const cpu = getSelfCpuUs(row);
  if (wall == null || cpu == null) return null;
  return Math.max(0, wall - cpu);
}

export function flattenTree(nodes: TreeNode[]): TreeNode[] {
  const rows: TreeNode[] = [];
  const walk = (node: TreeNode) => {
    rows.push(node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return rows;
}

export function buildTreeIndex(nodes: TreeNode[]) {
  const byId = new Map<string | number, TreeNode>();
  nodes.forEach((node) => {
    byId.set(node.id, { ...node, data: [] });
  });
  const roots: TreeNode[] = [];
  byId.forEach((node) => {
    if (node.parent_id && byId.has(node.parent_id)) {
      byId.get(node.parent_id)!.data!.push(node);
    } else {
      roots.push(node);
    }
  });
  const sortByPath = (a: TreeNode, b: TreeNode) => (a.sort_path || "").localeCompare(b.sort_path || "");
  roots.sort(sortByPath);
  byId.forEach((node) => node.data!.sort(sortByPath));
  return { roots, byId };
}

export function pruneTree(nodes: TreeNode[], minWallUs?: number | null): TreeNode[] {
  if (!minWallUs) return nodes.map((node) => ({ ...node, data: pruneTree(node.data || [], minWallUs) }));
  return nodes
    .map((node) => {
      const children = pruneTree(node.data || [], minWallUs);
      const wall = getWallUs(node);
      const keep = (wall != null && wall >= minWallUs) || children.length > 0;
      return keep ? { ...node, data: children } : null;
    })
    .filter(Boolean) as TreeNode[];
}

export function indexTree(nodes: TreeNode[]) {
  const byId = new Map<string | number, TreeNode>();
  const walk = (node: TreeNode) => {
    byId.set(node.id, node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return byId;
}

export function applyFocus(roots: TreeNode[], byId: Map<string | number, TreeNode>, focusId?: string | number | null) {
  if (focusId == null || focusId === "") return { roots, byId, breadcrumb: [] as TreeNode[] };
  const focusNode = byId.get(focusId);
  if (!focusNode) return { roots, byId, breadcrumb: [] as TreeNode[] };
  const breadcrumb: TreeNode[] = [];
  let current: TreeNode | undefined = focusNode;
  while (current) {
    breadcrumb.unshift(current);
    if (!current.parent_id) break;
    current = byId.get(current.parent_id) || undefined;
  }
  return { roots: [{ ...focusNode, data: focusNode.data || [] }], byId, breadcrumb };
}

export function applyHotPath(nodes: TreeNode[], mode: "off" | "self" | "total"): TreeNode[] {
  if (mode === "off") return nodes.map((node) => ({ ...node, data: applyHotPath(node.data || [], mode) }));
  const metric = (node: TreeNode) => (mode === "self" ? getSelfWallUs(node) ?? 0 : getWallUs(node) ?? 0);
  return nodes.map((node) => {
    if (!node.data || node.data.length === 0) return { ...node, data: [] };
    const sorted = [...node.data].sort((a, b) => metric(b) - metric(a));
    const best = sorted[0];
    const hidden = node.data.length - 1;
    return {
      ...node,
      hidden_siblings: hidden,
      data: best ? applyHotPath([best], mode) : [],
    };
  });
}

export function applyChainCompression(nodes: TreeNode[], { enabled, expandedChains, significantSelfUs }: ChainCompressionOptions) {
  const isSignificant = (node: TreeNode) =>
    significantSelfUs != null && (getSelfWallUs(node) ?? 0) >= significantSelfUs;

  const maxKeys = new Set(["wall_time_us", "est_wall_us", "cpu_time_us", "est_cpu_us"]);
  const skipKeys = new Set([
    "id",
    "parent_id",
    "depth",
    "open",
    "data",
    "sort_path",
    "span_name",
    "tag",
    "chain_count",
    "chain_label",
    "chain_full_label",
    "chain_tags",
    "chain_segments",
    "flame_percent",
  ]);

  const aggregateNumeric = (items: TreeNode[]) => {
    if (items.length === 0) return {} as Record<string, number>;
    const aggregate: Record<string, number> = {};
    items.forEach((item) => {
      Object.entries(item).forEach(([key, value]) => {
        if (skipKeys.has(key)) return;
        if (typeof value !== "number") return;
        if (maxKeys.has(key)) {
          aggregate[key] = Math.max(aggregate[key] ?? 0, value);
        } else {
          aggregate[key] = (aggregate[key] ?? 0) + value;
        }
      });
    });
    return aggregate;
  };

  const compressNode = (node: TreeNode): TreeNode => {
    if (!enabled || expandedChains.has(node.id) || isSignificant(node)) {
      return { ...node, data: (node.data || []).map(compressNode) };
    }
    const chain: TreeNode[] = [node];
    let cursor: TreeNode = node;
    while (
      cursor.data &&
      cursor.data.length === 1 &&
      !expandedChains.has(cursor.data[0].id) &&
      !isSignificant(cursor.data[0])
    ) {
      cursor = cursor.data[0];
      chain.push(cursor);
    }
    const tailChildren = (cursor.data || []).map(compressNode);
    if (chain.length <= 1) return { ...node, data: tailChildren };
    const labelSegments = chain.map((item) => item.span_name ?? "-");
    const chainLabel = labelSegments.join(" › ");
    const chainTags = Array.from(
      new Set(chain.map((item) => item.tag).filter((tag) => tag && String(tag).trim().length > 0))
    );
    const chainSegments = chain.map((item) => ({
      id: item.id,
      name: item.span_name ?? "-",
      tag: item.tag ?? null,
      span_context: item.span_context ?? null,
      // Per-segment metrics for hover cards
      call_count: item.call_count ?? null,
      sample_count: item.sample_count ?? null,
      wall_us: getWallUs(item),
      self_wall_us: getSelfWallUs(item),
      cpu_us: item.est_cpu_us ?? item.cpu_time_us ?? null,
      self_cpu_us: getSelfCpuUs(item),
    }));
    const aggregated = aggregateNumeric(chain);
    return {
      ...node,
      ...aggregated,
      data: tailChildren,
      chain_count: chain.length - 1,
      chain_label: chainLabel,
      chain_full_label: chainLabel,
      chain_tags: chainTags,
      chain_segments: chainSegments,
    };
  };

  return nodes.map(compressNode);
}

// ---------------------------------------------------------------------------
// In-memory tree filter: remove & re-parent
// ---------------------------------------------------------------------------

/** Classify a span_context into a Clarity function type label, or null. */
function classifyClarityFnType(ctx: string | null | undefined): string | null {
  if (!ctx) return null;
  if (ctx === "clarity::builtin") return "Built-In";
  if (ctx.startsWith("clarity::user::")) {
    const vis = ctx.slice("clarity::user::".length);
    if (vis === "public") return "Public";
    if (vis === "private") return "Private";
    if (vis === "read-only" || vis === "read_only") return "Read-Only";
    return "Public"; // fallback
  }
  return null;
}

/** Returns true if context is one of the Clarity pseudo-contexts. */
export function isClarityPseudoContext(ctx: string | null | undefined): boolean {
  if (!ctx) return false;
  return ctx === "clarity::builtin" || ctx === "clarity::dispatch" || ctx.startsWith("clarity::user::");
}

/** Map from FilterBuilder field id → accessor that extracts the value from a TreeNode. */
const TRACE_FIELD_ACCESSORS: Record<string, (node: TreeNode) => string | number | null> = {
  span_name:           (n) => n.span_name ?? null,
  tag:                 (n) => n.tag ?? null,
  span_context:        (n) => n.span_context ?? null,
  clarity_fn_type:     (n) => classifyClarityFnType(n.span_context),
  clarity_dispatch_type: (n) => n.span_context === "clarity::dispatch" ? (n.span_name ?? null) : null,
  clarity_builtin_fn:  (n) => n.span_context === "clarity::builtin" ? (n.span_name ?? null) : null,
  clarity_user_fn:     (n) => n.span_context?.startsWith("clarity::user::") ? (n.tag ?? n.span_name ?? null) : null,
  profiler_span_id:    (n) => n.profiler_span_id != null ? String(n.profiler_span_id) : null,
  call_count:          (n) => n.call_count ?? null,

  // Wall Time (ms)
  wall_inc_total:      (n) => { const v = getWallUs(n); return v != null ? v / 1000 : null; },
  wall_inc_avg:        (n) => toAvgMs(getWallUs(n), n as any),
  wall_self_total:     (n) => { const v = getSelfWallUs(n); return v != null ? v / 1000 : null; },
  wall_self_avg:       (n) => toAvgMs(getSelfWallUs(n), n as any),

  // Busy Time (ms) – CPU time
  busy_inc_total:      (n) => { const v = n.est_cpu_us ?? n.cpu_time_us; return v != null ? v / 1000 : null; },
  busy_inc_avg:        (n) => toAvgMs(n.est_cpu_us ?? n.cpu_time_us, n as any),
  busy_self_total:     (n) => { const v = getSelfCpuUs(n); return v != null ? v / 1000 : null; },
  busy_self_avg:       (n) => toAvgMs(getSelfCpuUs(n), n as any),

  // Wait Time (ms) – wall minus CPU
  wait_inc_total:      (n) => { const wall = getWallUs(n); const cpu = n.est_cpu_us ?? n.cpu_time_us; return (wall != null && cpu != null) ? Math.max(0, wall - cpu) / 1000 : null; },
  wait_inc_avg:        (n) => { const wall = getWallUs(n); const cpu = n.est_cpu_us ?? n.cpu_time_us; return (wall != null && cpu != null) ? toAvgMs(Math.max(0, wall - cpu), n as any) : null; },
  wait_self_total:     (n) => { const v = getSelfWaitUs(n); return v != null ? v / 1000 : null; },
  wait_self_avg:       (n) => { const v = getSelfWaitUs(n); return v != null ? toAvgMs(v, n as any) : null; },

  // Clarity metrics – Total
  clarity_runtime_total:      (n) => n.clarity_runtime_total ?? null,
  clarity_input_n_total:      (n) => n.clarity_input_n_total ?? null,
  clarity_read_count_total:   (n) => n.clarity_read_count_total ?? null,
  clarity_read_length_total:  (n) => n.clarity_read_length_total ?? null,
  clarity_write_count_total:  (n) => n.clarity_write_count_total ?? null,
  clarity_write_length_total: (n) => n.clarity_write_length_total ?? null,

  // Clarity metrics – Avg
  clarity_runtime_avg:        (n) => n.clarity_runtime_avg ?? null,
  clarity_input_n_avg:        (n) => n.clarity_input_n_avg ?? null,
  clarity_read_count_avg:     (n) => n.clarity_read_count_avg ?? null,
  clarity_read_length_avg:    (n) => n.clarity_read_length_avg ?? null,
  clarity_write_count_avg:    (n) => n.clarity_write_count_avg ?? null,
  clarity_write_length_avg:   (n) => n.clarity_write_length_avg ?? null,
};

/** Test a single rule against a node. Returns true if the node matches. */
function testTextOp(haystack: string, needle: string, operator: string): boolean {
  switch (operator) {
    case "contains":    return haystack.includes(needle);
    case "notContains": return !haystack.includes(needle);
    case "equal":       return haystack === needle;
    case "notEqual":    return haystack !== needle;
    case "beginsWith":  return haystack.startsWith(needle);
    case "endsWith":    return haystack.endsWith(needle);
    default: return true;
  }
}

function testRule(node: TreeNode, rule: TreeFilterRule): boolean {
  let accessor = TRACE_FIELD_ACCESSORS[rule.field];
  if (!accessor) return true; // Unknown field → don't filter

  // For span_name is/isNot, values contain profiler_span_id strings, so
  // switch to the profiler_span_id accessor for exact identity matching.
  if (rule.field === "span_name" && (rule.operator === "is" || rule.operator === "isNot")) {
    accessor = TRACE_FIELD_ACCESSORS["profiler_span_id"] ?? accessor;
  }

  const raw = accessor(node);

  const { operator } = rule;

  // Multi-value: the FilterBuilder stores chips in values[] for
  // contains / equal / beginsWith / endsWith as well as enum is/isNot.
  // For text ops the semantics are OR across values (match ANY).
  if (rule.values && rule.values.length > 0) {
    const haystack = (raw != null ? String(raw) : "").toLowerCase();
    if (operator === "is" || operator === "isNot") {
      const matchesAny = rule.values.some((v) => v.toLowerCase() === haystack);
      return operator === "isNot" ? !matchesAny : matchesAny;
    }
    // Text ops with multi-value chips: OR across all values
    const matchesAny = rule.values.some((v) =>
      testTextOp(haystack, v.toLowerCase(), operator)
    );
    // For negated ops (notContains, notEqual) we want ALL to not-match,
    // which is already handled: testTextOp returns false when it does match,
    // so matchesAny is true only when at least one value makes the negated
    // check pass. Just return the OR result directly.
    return matchesAny;
  }

  if (rule.value == null || rule.value === "") return true;

  const { value } = rule;

  const NUMERIC_ONLY_OPS = new Set(["greater", "greaterOrEqual", "less", "lessOrEqual"]);
  const isNumericOp = NUMERIC_ONLY_OPS.has(operator) || (typeof raw === "number");

  // Numeric operators — check FIRST because value from FilterBuilder is always
  // a string (e.g. "50") which would otherwise match the text branch below.
  // Null/undefined raw with a numeric operator: the node has no data for this
  // field, so it should not satisfy the comparison (except notEqual).
  if (isNumericOp) {
    if (raw == null) {
      return operator === "notEqual";
    }
    const numVal = Number(raw);
    let numTarget = Number(value);
    if (Number.isNaN(numVal) || Number.isNaN(numTarget)) return true;

    // Convert the user's target value to ms (all time accessors return ms).
    switch (rule.modifier) {
      case "us": numTarget = numTarget / 1000; break;   // μs → ms
      case "s":  numTarget = numTarget * 1000; break;    // s  → ms
      // "ms" or undefined: no conversion needed
    }

    switch (operator) {
      case "equal":          return numVal === numTarget;
      case "notEqual":       return numVal !== numTarget;
      case "greater":        return numVal > numTarget;
      case "greaterOrEqual": return numVal >= numTarget;
      case "less":           return numVal < numTarget;
      case "lessOrEqual":    return numVal <= numTarget;
      default: break;
    }
    return true;
  }

  // Text operators (including is/isNot with single value)
  if (typeof raw === "string" || raw != null) {
    const haystack = (raw != null ? String(raw) : "").toLowerCase();
    const needle = String(value).toLowerCase();
    if (operator === "is") return haystack === needle;
    if (operator === "isNot") return haystack !== needle;
    return testTextOp(haystack, needle, operator);
  }

  return true;
}

/** Test a filter group (AND/OR of rules + nested groups) against a node. */
function testFilter(node: TreeNode, filter: TreeFilterGroup): boolean {
  if (!filter.rules || filter.rules.length === 0) return true;

  const test = (item: TreeFilterRule | TreeFilterGroup): boolean => {
    if ("rules" in item && Array.isArray(item.rules)) {
      return testFilter(node, item as TreeFilterGroup);
    }
    return testRule(node, item as TreeFilterRule);
  };

  return filter.glue === "or"
    ? filter.rules.some(test)
    : filter.rules.every(test);
}

/**
 * Apply client-side filter to a tree using remove & re-parent semantics:
 * - If a node does NOT match the filter, it is removed and its children
 *   are promoted into the removed node's parent.
 * - If a node DOES match, it is kept along with all its descendants.
 * - Nodes that had children removed get a `_filtered_children` count.
 *
 * Returns { roots, totalFiltered } where totalFiltered is the number of
 * nodes removed across the entire tree.
 */
export function applyTreeFilter(
  nodes: TreeNode[],
  filter: TreeFilterGroup | null
): { roots: TreeNode[]; totalFiltered: number } {
  if (!filter || !filter.rules || filter.rules.length === 0) {
    return { roots: nodes, totalFiltered: 0 };
  }

  let totalFiltered = 0;

  function filterNode(node: TreeNode): TreeNode[] {
    // First, recursively process all children
    const processedChildren: TreeNode[] = [];
    for (const child of node.data || []) {
      processedChildren.push(...filterNode(child));
    }

    const matches = testFilter(node, filter!);

    if (matches) {
      // Node matches: keep it with its processed children
      const filteredChildCount = (node.data?.length ?? 0) - processedChildren.filter(
        (c) => (node.data || []).some((orig) => orig.id === c.id)
      ).length;
      return [{
        ...node,
        data: processedChildren,
        _filtered_children: filteredChildCount > 0 ? filteredChildCount : undefined,
      }];
    } else {
      // Node doesn't match: remove it, promote its processed children
      totalFiltered++;
      return processedChildren;
    }
  }

  const roots: TreeNode[] = [];
  for (const node of nodes) {
    roots.push(...filterNode(node));
  }

  return { roots, totalFiltered };
}

export function applyOpenState(nodes: TreeNode[], openNodes: Set<string | number>, forceOpenAll: boolean) {
  const walk = (node: TreeNode, depth: number): TreeNode => {
    const open = forceOpenAll ? true : depth === 0 || openNodes.has(node.id);
    return {
      ...node,
      open,
      data: (node.data || []).map((child) => walk(child, depth + 1)),
    };
  };
  return nodes.map((node) => walk(node, 0));
}

export function collectSubtreeIds(node: TreeNode, ids: Set<string | number>) {
  ids.add(node.id);
  (node.data || []).forEach((child) => collectSubtreeIds(child, ids));
}

export function computeDefaultOpenSet(nodes: TreeNode[], defaultAutoExpand: DefaultAutoExpand) {
  const openIds = new Set<string | number>();
  const walk = (node: TreeNode, depth: number) => {
    const wallMs = (getWallUs(node) ?? 0) / 1000;
    const selfMs = (getSelfWallUs(node) ?? 0) / 1000;
    if (
      depth < defaultAutoExpand.depth ||
      selfMs >= defaultAutoExpand.selfMs ||
      wallMs >= defaultAutoExpand.wallMs
    ) {
      openIds.add(node.id);
    }
    if (node.data && node.data.length > 0) {
      const sorted = [...node.data].sort((a, b) => (getWallUs(b) ?? 0) - (getWallUs(a) ?? 0));
      sorted.slice(0, defaultAutoExpand.topKChildren).forEach((child) => openIds.add(child.id));
      node.data.forEach((child) => walk(child, depth + 1));
    }
  };
  nodes.forEach((node) => walk(node, 0));
  return openIds;
}
