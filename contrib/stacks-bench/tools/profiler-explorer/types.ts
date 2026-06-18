// ---------------------------------------------------------------------------
// Shared type definitions for the Profiler Explorer backend
// ---------------------------------------------------------------------------

// ── Filter DSL types ────────────────────────────────────────────────────────

/** A comparison operator mapping to a SQL comparison. */
export type ComparisonOp = "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte";

/** A pattern operator mapping to SQL LIKE / NOT LIKE. */
export type PatternOp = "$contains" | "$ncontains" | "$startsWith" | "$endsWith";

/** Set operators for IN/NOT IN queries. */
export type SetOp = "$in" | "$nin";

/** All supported filter operators. */
export type FilterOp = ComparisonOp | PatternOp | SetOp;

/** Field names accepted by the filter DSL. */
export type FilterField =
  | "contract_issuer"
  | "contract_name"
  | "contract_fn"
  | "tx_hash_hex"
  | "stacks_block_height"
  | "duration_ms"
  | "clarity_runtime"
  | "clarity_read_count"
  | "clarity_read_length"
  | "clarity_write_count"
  | "clarity_write_length"
  | "tx_type_name";

/** An operator → value mapping inside a leaf filter node. */
export type OpValue = {
  [K in ComparisonOp | PatternOp]?: string | number;
} & {
  [K in SetOp]?: (string | number)[];
};

/** A leaf filter node: one field with one operator. */
export type FilterLeaf = {
  [field in FilterField]?: OpValue;
};

/** A compound filter node with $and / $or combinators. */
export interface FilterAnd {
  $and: FilterNode[];
}

export interface FilterOr {
  $or: FilterNode[];
}

/** Any node in the filter tree. */
export type FilterNode = FilterLeaf | FilterAnd | FilterOr;

/** The result of building a WHERE clause from a filter tree. */
export interface WhereClause {
  sql: string;
  params: Record<string, string | number>;
}

// ── Database row types ──────────────────────────────────────────────────────

export interface RunRow {
  id: number;
  label: string;
  stacks_node_version: string | null;
  block_count: number;
  tx_count: number;
  created_at: string;
}

export interface BlockRow {
  stacks_block_id: number;
  height: number;
  hash: string | null;
  index_block_hash: string | null;
  tx_count: number;
}

export interface TxLookupResult {
  stacks_tx_id: number;
}

export interface TransactionRow {
  stacks_tx_id: number;
  tx_hash_hex: string;
  tx_type_name: string | null;
  contract_issuer: string | null;
  contract_name: string | null;
  contract_fn: string | null;
  stacks_block_height: number | null;
  duration_ms: number | null;
  clarity_runtime: number | null;
  clarity_read_count: number | null;
  clarity_read_length: number | null;
  clarity_write_count: number | null;
  clarity_write_length: number | null;
}

export interface TransactionMaxes {
  duration_ms: number;
  clarity_runtime: number;
  clarity_read_count: number;
  clarity_read_length: number;
  clarity_write_count: number;
  clarity_write_length: number;
}

export interface TxTypeRow {
  name: string;
}

export interface AutocompleteResult {
  values: string[];
}

export interface TraceRecord {
  id: number;
  parent_id: number | null;
  span_name: string;
  context: string | null;
  tag_name: string | null;
  depth: number;
  duration_us: number;
  busy_us: number;
  wait_us: number;
  samples: number;
  clarity_runtime: number | null;
  clarity_read_count: number | null;
  clarity_read_length: number | null;
  clarity_write_count: number | null;
  clarity_write_length: number | null;
  kv_get_count: number | null;
  kv_get_bytes: number | null;
  kv_set_count: number | null;
  kv_set_bytes: number | null;
  kv_delete_count: number | null;
  kv_delete_bytes: number | null;
}

export interface RecordKvItem {
  key_hex: string;
  key_utf8: string;
  op: string;
  value_len: number;
}
