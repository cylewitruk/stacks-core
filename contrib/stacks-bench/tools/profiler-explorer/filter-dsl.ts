// ---------------------------------------------------------------------------
// MongoDB-style filter DSL → parameterized SQL translator
// ---------------------------------------------------------------------------
import type { FilterField, FilterNode, WhereClause } from "./types.ts";

// Accepts filter trees like:
//   { "$and": [ { "contract_name": { "$contains": "foo" } }, { "duration_ms": { "$gte": 100 } } ] }
//   { "$or": [ { "contract_issuer": { "$eq": "SP2..." } }, { "contract_fn": { "$startsWith": "mint" } } ] }
//
// Comparison operators: $eq, $ne, $gt, $gte, $lt, $lte
// Pattern operators:    $contains, $ncontains, $startsWith, $endsWith
//   (the backend wraps values with SQL wildcards automatically)
// Logical combinators:  $and, $or

/** Direct-comparison operators that map 1:1 to SQL operators. */
const COMPARISON_OP_MAP: Record<string, string> = {
  $eq: "=",
  $ne: "!=",
  $gt: ">",
  $gte: ">=",
  $lt: "<",
  $lte: "<=",
};

/**
 * Pattern operators: the backend wraps the user-supplied value with the
 * appropriate SQL wildcards so the public API never leaks SQL syntax.
 *   { sqlOp, wrap: (value) => wrappedValue }
 */
const PATTERN_OP_MAP: Record<string, { sqlOp: string; wrap: (v: string) => string }> = {
  $contains:   { sqlOp: "LIKE",     wrap: (v: string) => `%${v}%` },
  $ncontains:  { sqlOp: "NOT LIKE", wrap: (v: string) => `%${v}%` },
  $startsWith: { sqlOp: "LIKE",     wrap: (v: string) => `${v}%` },
  $endsWith:   { sqlOp: "LIKE",     wrap: (v: string) => `%${v}` },
};

// Map user-facing field names → SQL expressions
export const FILTER_FIELD_MAP: Record<FilterField, string> = {
  contract_issuer: "p.address",
  contract_name: "c.name",
  contract_fn: "cf.name",
  tx_hash_hex: "tx.tx_hash_hex",
  stacks_block_height: "sb.height",
  duration_ms: "(sts.duration_us / 1000.0)",
  clarity_runtime: "sts.clarity_runtime",
  clarity_read_count: "sts.clarity_read_count",
  clarity_read_length: "sts.clarity_read_length",
  clarity_write_count: "sts.clarity_write_count",
  clarity_write_length: "sts.clarity_write_length",
  tx_type_name: "ttype.name",
};

export const ALLOWED_FIELDS = new Set(Object.keys(FILTER_FIELD_MAP));
export const ALLOWED_OPS = new Set([
  ...Object.keys(COMPARISON_OP_MAP),
  ...Object.keys(PATTERN_OP_MAP),
  "$in",
  "$nin",
]);

let _paramIdx = 0;

/** Reset the parameter counter (for deterministic tests). */
export function resetParamCounter() {
  _paramIdx = 0;
}

/**
 * Recursively convert a filter node into { sql, params }.
 * @param {object} node  – a filter tree node
 * @returns {{ sql: string, params: Record<string, any> }}
 */
export function buildWhere(node: FilterNode): WhereClause {
  if (!node || typeof node !== "object") {
    throw Object.assign(new Error("Invalid filter node"), { status: 400 });
  }

  if ("$and" in node) {
    const andNode = node as { $and: FilterNode[] };
    if (!Array.isArray(andNode.$and) || andNode.$and.length === 0) {
      return { sql: "1=1", params: {} };
    }
    const parts = andNode.$and.map(buildWhere);
    return {
      sql: `(${parts.map((p) => p.sql).join(" AND ")})`,
      params: Object.assign({}, ...parts.map((p) => p.params)),
    };
  }

  if ("$or" in node) {
    const orNode = node as { $or: FilterNode[] };
    if (!Array.isArray(orNode.$or) || orNode.$or.length === 0) {
      return { sql: "1=1", params: {} };
    }
    const parts = orNode.$or.map(buildWhere);
    return {
      sql: `(${parts.map((p) => p.sql).join(" OR ")})`,
      params: Object.assign({}, ...parts.map((p) => p.params)),
    };
  }

  // Leaf node: { "field": { "$op": value } }
  const entries = Object.entries(node);
  if (entries.length !== 1) {
    throw Object.assign(new Error("Filter leaf must have exactly one field"), { status: 400 });
  }
  const [field, ops] = entries[0] as [string, Record<string, unknown>];
  if (!ALLOWED_FIELDS.has(field)) {
    throw Object.assign(new Error(`Invalid filter field: ${field}`), { status: 400 });
  }
  const opEntries = Object.entries(ops);
  if (opEntries.length !== 1) {
    throw Object.assign(new Error("Filter leaf must have exactly one operator"), { status: 400 });
  }
  const [op, value] = opEntries[0] as [string, unknown];
  if (!ALLOWED_OPS.has(op)) {
    throw Object.assign(new Error(`Invalid filter operator: ${op}`), { status: 400 });
  }

  const paramName = `_fp${_paramIdx++}`;
  const sqlField = FILTER_FIELD_MAP[field as FilterField];

  // $in operator: value must be an array → generates field IN (:p0, :p1, ...)
  if (op === "$in") {
    if (!Array.isArray(value) || value.length === 0) {
      throw Object.assign(new Error("$in requires a non-empty array value"), { status: 400 });
    }
    const paramNames = value.map(() => `_fp${_paramIdx++}`);
    const params: Record<string, string | number> = {};
    for (let i = 0; i < value.length; i++) {
      params[paramNames[i]] = value[i];
    }
    return {
      sql: `${sqlField} IN (${paramNames.map((p) => `:${p}`).join(", ")})`,
      params,
    };
  }

  // $nin operator: value must be an array → generates field NOT IN (:p0, :p1, ...)
  if (op === "$nin") {
    if (!Array.isArray(value) || value.length === 0) {
      throw Object.assign(new Error("$nin requires a non-empty array value"), { status: 400 });
    }
    const paramNames = value.map(() => `_fp${_paramIdx++}`);
    const params: Record<string, string | number> = {};
    for (let i = 0; i < value.length; i++) {
      params[paramNames[i]] = value[i];
    }
    return {
      sql: `${sqlField} NOT IN (${paramNames.map((p) => `:${p}`).join(", ")})`,
      params,
    };
  }

  // Pattern operators wrap the value with SQL wildcards automatically
  const pattern = PATTERN_OP_MAP[op];
  if (pattern) {
    return {
      sql: `${sqlField} ${pattern.sqlOp} :${paramName}`,
      params: { [paramName]: pattern.wrap(String(value)) },
    };
  }

  // Direct comparison operators
  return {
    sql: `${sqlField} ${COMPARISON_OP_MAP[op]} :${paramName}`,
    params: { [paramName]: value as string | number },
  };
}

/**
 * Parse the `filter` query param (JSON string) and return { sql, params }.
 * Returns null if no filter is provided.
 */
export function parseFilterParam(filterStr: string | undefined | null): WhereClause | null {
  if (!filterStr) return null;
  let parsed;
  try {
    parsed = JSON.parse(filterStr);
  } catch {
    throw Object.assign(new Error("Invalid filter JSON"), { status: 400 });
  }
  // Depth / complexity guard
  const depth = (n: Record<string, unknown[]>, d: number): void => {
    if (d > 6) throw Object.assign(new Error("Filter too deeply nested (max 6)"), { status: 400 });
    if (n.$and) (n.$and as Record<string, unknown[]>[]).forEach((c) => depth(c, d + 1));
    if (n.$or) (n.$or as Record<string, unknown[]>[]).forEach((c) => depth(c, d + 1));
  };
  depth(parsed, 0);
  return buildWhere(parsed);
}
