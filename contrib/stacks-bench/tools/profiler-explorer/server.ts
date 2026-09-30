import express, { type Request, type Response } from "express";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";
import Database from "better-sqlite3";
import type { FilterField } from "./types.ts";

const DEFAULT_DB_RELATIVE = ".stacks-bench/appdata/stacks-bench.db";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const distDir = path.join(__dirname, "dist");

function resolveDbPath(dbArg: string | null): string {
  if (dbArg) return path.resolve(dbArg);
  if (process.env.STACKS_BENCH_DB) return path.resolve(process.env.STACKS_BENCH_DB);
  return path.resolve(process.cwd(), DEFAULT_DB_RELATIVE);
}

function parseOptionalInt(value: unknown, name: string): number | null {
  if (value === undefined || value === null || value === "") return null;
  const parsed = Number(value);
  if (!Number.isInteger(parsed)) {
    const error = new Error(`Invalid ${name}: ${value}`) as Error & { status: number };
    error.status = 400;
    throw error;
  }
  return parsed;
}

function parseCsvList(value: unknown): string[] {
  if (!value) return [];
  return String(value)
    .split(",")
    .map((entry) => entry.trim())
    .filter(Boolean);
}

function openDb(dbPath: string): Database.Database {
  return new Database(dbPath, { fileMustExist: true });
}

function queryAll<T = Record<string, unknown>>(db: Database.Database, sql: string, params: Record<string, unknown>): T[] {
  return db.prepare(sql).all(params) as T[];
}

// ---------------------------------------------------------------------------
// MongoDB-style filter DSL → parameterized SQL translator
// ---------------------------------------------------------------------------

import {
  FILTER_FIELD_MAP,
  ALLOWED_FIELDS,
  parseFilterParam,
} from "./filter-dsl.ts";

function traceSqlTxMode() {
  return `
    WITH RECURSIVE
    bench AS (
      SELECT id AS benchmark_run_id
      FROM benchmark_run
      WHERE id = :run_id
    ),
    tx_seed AS (
      SELECT pr.benchmark_run_id, pr.id
      FROM profiler_record pr
      JOIN bench b ON b.benchmark_run_id = pr.benchmark_run_id
      WHERE (:stacks_tx_id IS NULL OR pr.stacks_tx_id = :stacks_tx_id)
    ),
    ancestors AS (
      SELECT benchmark_run_id, id
      FROM tx_seed
      UNION ALL
      SELECT p.benchmark_run_id, p.parent_id
      FROM profiler_record p
      JOIN ancestors a
        ON p.benchmark_run_id = a.benchmark_run_id
       AND p.id = a.id
      WHERE p.parent_id IS NOT NULL
    ),
    descendants AS (
      SELECT benchmark_run_id, id
      FROM tx_seed
      UNION ALL
      SELECT c.benchmark_run_id, c.id
      FROM profiler_record c
      JOIN descendants p
        ON c.benchmark_run_id = p.benchmark_run_id
       AND c.parent_id = p.id
    ),
    kept AS (
      SELECT benchmark_run_id, id FROM ancestors
      UNION
      SELECT benchmark_run_id, id FROM descendants
    ),
    payload AS (
      SELECT pr.*, synth.stacks_block_id
      FROM kept k
      JOIN profiler_record pr
        ON pr.benchmark_run_id = k.benchmark_run_id
       AND pr.id = k.id
      JOIN synthetic_block synth
        ON synth.id = pr.synthetic_block_id
    ),
    kv_counts AS (
      SELECT k.id AS id,
             COUNT(prkv.profiler_record_id) AS kv_pairs,
             COALESCE(SUM(prkv.count), 0) AS kv_total
      FROM kept k
      LEFT JOIN profiler_record_kv prkv
        ON prkv.profiler_record_id = k.id
      GROUP BY k.id
    ),
    trace_tree AS (
      SELECT p.*, printf('%09d', p.id) AS sort_path
      FROM payload p
      LEFT JOIN kept pk
        ON pk.benchmark_run_id = p.benchmark_run_id
       AND pk.id = p.parent_id
      WHERE pk.id IS NULL
      UNION ALL
      SELECT c.*, t.sort_path || '.' || printf('%09d', c.child_index) AS sort_path
      FROM payload c
      JOIN trace_tree t
        ON c.benchmark_run_id = t.benchmark_run_id
       AND c.parent_id = t.id
    )
    SELECT
      t.id,
      t.parent_id,
      t.child_index,
      t.depth,
      t.profiler_span_id,
      t.profiler_tag_id,
      t.synthetic_block_id,
      t.stacks_tx_id,
      t.benchmark_run_id,

      t.call_count,
      t.sample_count,
      t.wall_time_us,
      t.self_wall_time_us,
      t.cpu_time_us,
      t.self_cpu_time_us,
      t.expand_factor,
      t.est_wall_us,
      t.est_self_wall_us,
      t.est_cpu_us,
      t.est_self_cpu_us,

      s.name AS span_name,
      s.context AS span_context,
      pt.tag AS tag,
      tx.tx_hash_hex,
      b.block_hash_hex,
      p.address AS contract_issuer,
      c.name AS contract,
      c_fn.name AS contract_fn,

      COALESCE(kvc.kv_pairs, 0) AS kv_pairs,
      COALESCE(kvc.kv_total, 0) AS kv_total,

      prcc.runtime AS clarity_runtime_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.runtime * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_runtime_avg,
      prcc.read_count AS clarity_read_count_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.read_count * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_read_count_avg,
      prcc.read_length AS clarity_read_length_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.read_length * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_read_length_avg,
      prcc.write_count AS clarity_write_count_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.write_count * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_write_count_avg,
      prcc.write_length AS clarity_write_length_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.write_length * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_write_length_avg,
      prcc.input_n AS clarity_input_n_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.input_n * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_input_n_avg,

      t.sort_path
    FROM trace_tree t
    JOIN profiler_span s ON s.id = t.profiler_span_id
    LEFT JOIN profiler_tag pt ON pt.id = t.profiler_tag_id
    LEFT JOIN stacks_block b ON b.id = t.stacks_block_id
    LEFT JOIN stacks_tx tx ON tx.id = t.stacks_tx_id
    LEFT JOIN contract c ON c.id = tx.contract_id
    LEFT JOIN contract_fn c_fn ON c_fn.id = tx.contract_fn_id
    LEFT JOIN principal p ON p.id = c.issuer_principal_id
    LEFT JOIN kv_counts kvc ON kvc.id = t.id
    LEFT JOIN profiler_record_clarity_costs prcc ON prcc.profiler_record_id = t.id
    ORDER BY t.sort_path ASC
    `;
}

function traceSqlRunMode() {
  return `
    WITH RECURSIVE
    bench AS (
      SELECT id AS benchmark_run_id
      FROM benchmark_run
      WHERE id = :run_id
    ),
    run_scope AS (
      SELECT pr.*, synth.stacks_block_id
      FROM profiler_record pr
      JOIN bench b ON b.benchmark_run_id = pr.benchmark_run_id
      JOIN synthetic_block synth ON synth.id = pr.synthetic_block_id
      WHERE (:stacks_tx_id IS NULL OR pr.stacks_tx_id = :stacks_tx_id)
        AND (:stacks_block_id IS NULL OR synth.stacks_block_id = :stacks_block_id)
        AND (:min_wall_us IS NULL OR COALESCE(pr.est_wall_us, pr.wall_time_us) >= :min_wall_us)
    ),
    trace_tree AS (
      SELECT base.*, printf('%09d', base.id) AS sort_path
      FROM run_scope base
      WHERE base.parent_id IS NULL
        AND (:segment_root_id IS NULL OR base.id = :segment_root_id)
      UNION ALL
      SELECT child.*, parent.sort_path || '.' || printf('%09d', child.child_index) AS sort_path
      FROM run_scope child
      JOIN trace_tree parent
        ON child.parent_id = parent.id
       AND child.benchmark_run_id = parent.benchmark_run_id
    ),
    kv_counts AS (
      SELECT profiler_record_id,
             COUNT(*) AS kv_pairs,
             COALESCE(SUM(count), 0) AS kv_total
      FROM profiler_record_kv
      GROUP BY profiler_record_id
    )
    SELECT
      t.id,
      t.parent_id,
      t.child_index,
      t.depth,
      t.profiler_span_id,
      t.profiler_tag_id,
      t.synthetic_block_id,
      t.stacks_tx_id,
      t.benchmark_run_id,

      t.call_count,
      t.sample_count,
      t.wall_time_us,
      t.self_wall_time_us,
      t.cpu_time_us,
      t.self_cpu_time_us,
      t.expand_factor,
      t.est_wall_us,
      t.est_self_wall_us,
      t.est_cpu_us,
      t.est_self_cpu_us,

      s.name AS span_name,
      s.context AS span_context,
      pt.tag AS tag,
      tx.tx_hash_hex,
      b.block_hash_hex,
      p.address AS contract_issuer,
      c.name AS contract,
      c_fn.name AS contract_fn,

      COALESCE(kvc.kv_pairs, 0) AS kv_pairs,
      COALESCE(kvc.kv_total, 0) AS kv_total,

      prcc.runtime AS clarity_runtime_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.runtime * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_runtime_avg,
      prcc.read_count AS clarity_read_count_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.read_count * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_read_count_avg,
      prcc.read_length AS clarity_read_length_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.read_length * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_read_length_avg,
      prcc.write_count AS clarity_write_count_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.write_count * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_write_count_avg,
      prcc.write_length AS clarity_write_length_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.write_length * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_write_length_avg,
      prcc.input_n AS clarity_input_n_total,
      CASE
        WHEN t.sample_count > 0 THEN prcc.input_n * 1.0 / t.sample_count
        ELSE NULL
      END AS clarity_input_n_avg,

      t.sort_path
    FROM trace_tree t
    JOIN profiler_span s ON s.id = t.profiler_span_id
    LEFT JOIN profiler_tag pt ON pt.id = t.profiler_tag_id
    LEFT JOIN stacks_block b ON b.id = t.stacks_block_id
    LEFT JOIN stacks_tx tx ON tx.id = t.stacks_tx_id
    LEFT JOIN contract c ON c.id = tx.contract_id
    LEFT JOIN contract_fn c_fn ON c_fn.id = tx.contract_fn_id
    LEFT JOIN principal p ON p.id = c.issuer_principal_id
    LEFT JOIN kv_counts kvc ON kvc.profiler_record_id = t.id
    LEFT JOIN profiler_record_clarity_costs prcc ON prcc.profiler_record_id = t.id
    ORDER BY t.sort_path ASC
    LIMIT :limit
    `;
}

interface CliArgs {
  db: string | null;
  port: number;
  host: string;
}

function parseArgs(argv: string[]): CliArgs {
  const args: CliArgs = { db: null, port: 8800, host: "127.0.0.1" };
  for (let i = 0; i < argv.length; i += 1) {
    const value = argv[i];
    if (value === "--db") {
      args.db = argv[i + 1];
      i += 1;
    } else if (value === "--port") {
      args.port = Number(argv[i + 1]);
      i += 1;
    } else if (value === "--host") {
      args.host = argv[i + 1];
      i += 1;
    }
  }
  return args;
}

const { db: dbArg, port, host } = parseArgs(process.argv.slice(2));
const dbPath = resolveDbPath(dbArg);
if (!fs.existsSync(dbPath)) {
  console.error(`Database not found: ${dbPath}`);
  process.exit(1);
}

const db = openDb(dbPath);
const app = express();

app.get("/api/health", (_req: Request, res: Response) => {
  res.json({ ok: true });
});

app.get("/api/runs", (_req: Request, res: Response) => {
  const rows = queryAll(
    db,
    `
      SELECT id, run_name, start_time, end_time
      FROM benchmark_run
      ORDER BY id DESC
      LIMIT 200
    `,
    {}
  );
  res.json(rows);
});

app.get("/api/blocks", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    if (runId == null) return res.json([]);
    const rows = queryAll(
      db,
      `
        SELECT DISTINCT sb.id AS stacks_block_id,
               sb.block_hash_hex,
               sb.height
        FROM profiler_record pr
        JOIN synthetic_block synth ON synth.id = pr.synthetic_block_id
        JOIN stacks_block sb ON sb.id = synth.stacks_block_id
        WHERE pr.benchmark_run_id = :run_id
        ORDER BY sb.height DESC
        LIMIT 500
      `,
      { run_id: runId }
    );
    return res.json(rows);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

app.get("/api/txs", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const limit = parseOptionalInt(req.query.limit, "limit") || 200;
    const q = req.query.q;
    if (runId == null) return res.json([]);

    const params: Record<string, unknown> = { run_id: runId, limit };
    let filterSql = "";
    if (q) {
      params.q = `${String(q).toLowerCase()}%`;
      filterSql = "AND tx.tx_hash_hex LIKE :q";
    }

    const rows = queryAll(
      db,
      `
        SELECT tx.id AS stacks_tx_id,
               tx.tx_hash_hex,
               tx.stacks_block_id
        FROM stacks_tx tx
        JOIN profiler_record pr ON pr.stacks_tx_id = tx.id
        WHERE pr.benchmark_run_id = :run_id
          ${filterSql}
        GROUP BY tx.id
        ORDER BY tx.id DESC
        LIMIT :limit
      `,
      params
    );
    return res.json(rows);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

app.get("/api/tx-lookup", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const txHash = req.query.tx_hash;
    if (runId == null || !txHash) {
      return res.status(400).json({ error: "run_id and tx_hash are required" });
    }

    const rows = queryAll(
      db,
      `
        SELECT tx.id AS stacks_tx_id,
               tx.tx_hash_hex
        FROM stacks_tx tx
        JOIN profiler_record pr ON pr.stacks_tx_id = tx.id
        WHERE pr.benchmark_run_id = :run_id
          AND tx.tx_hash_hex = :tx_hash
        LIMIT 1
      `,
      { run_id: runId, tx_hash: String(txHash).toLowerCase() }
    );

    if (!rows.length) return res.json({ stacks_tx_id: null });
    return res.json(rows[0]);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Transaction browsing endpoint with pagination, filtering, and sorting
app.get("/api/transactions", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const offset = parseOptionalInt(req.query.offset, "offset") || 0;
    const limit = parseOptionalInt(req.query.limit, "limit") || 100;
    const sortBy = String(req.query.sort_by || "duration_ms");
    const sortDir = req.query.sort_dir === "asc" ? "ASC" : "DESC";

    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }

    // Validate sort column to prevent SQL injection
    const validSortColumns = [
      "duration_ms",
      "clarity_runtime",
      "clarity_read_count",
      "clarity_read_length",
      "clarity_write_count",
      "clarity_write_length",
      "stacks_block_height",
      "tx_hash_hex",
      "contract_issuer",
      "contract_name",
      "contract_fn",
    ];
    const safeSortBy = validSortColumns.includes(sortBy) ? sortBy : "duration_ms";

    // Build WHERE from filter DSL
    const conditions: string[] = ["sts.benchmark_run_id = :run_id"];
    const params: Record<string, unknown> = { run_id: runId, _limit: limit, _offset: offset };

    const filterResult = parseFilterParam(req.query.filter as string | undefined);
    if (filterResult) {
      conditions.push(filterResult.sql);
      Object.assign(params, filterResult.params);
    }

    const whereClause = conditions.join(" AND ");

    // Count total for pagination
    const countSql = `
      SELECT COUNT(*) as total
      FROM stacks_tx_stats sts
      JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
      LEFT JOIN contract c ON c.id = tx.contract_id
      LEFT JOIN principal p ON p.id = c.issuer_principal_id
      LEFT JOIN contract_fn cf ON cf.id = tx.contract_fn_id
      LEFT JOIN stacks_tx_type ttype ON ttype.id = tx.stacks_tx_type_id
      JOIN synthetic_block synth ON synth.id = sts.synthetic_block_id
      JOIN stacks_block sb ON sb.id = synth.stacks_block_id
      WHERE ${whereClause}
    `;
    const countResult = queryAll(db, countSql, params);
    const total = countResult[0]?.total || 0;

    // Sort field mapping (user-facing name → SQL expression)
    const sortFieldMap: Record<string, string> = {
      duration_ms: "duration_ms",
      clarity_runtime: "clarity_runtime",
      clarity_read_count: "clarity_read_count",
      clarity_read_length: "clarity_read_length",
      clarity_write_count: "clarity_write_count",
      clarity_write_length: "clarity_write_length",
      stacks_block_height: "stacks_block_height",
      tx_hash_hex: "tx_hash_hex",
      contract_issuer: "contract_issuer",
      contract_name: "contract_name",
      contract_fn: "contract_fn",
      tx_type_name: "tx_type_name",
    };
    const sortExpr = sortFieldMap[safeSortBy] || "duration_ms";

    // Main query
    const sql = `
      SELECT
        sts.benchmark_run_id,
        sts.synthetic_block_id,
        sts.stacks_tx_id,
        tx.tx_hash_hex,
        sb.height AS stacks_block_height,
        sb.block_hash_hex AS stacks_block_hash,
        p.address AS contract_issuer,
        c.name AS contract_name,
        cf.name AS contract_fn,
        ttype.name AS tx_type_name,
        sts.duration_us / 1000.0 AS duration_ms,
        sts.clarity_runtime,
        sts.clarity_read_count,
        sts.clarity_read_length,
        sts.clarity_write_count,
        sts.clarity_write_length
      FROM stacks_tx_stats sts
      JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
      JOIN synthetic_block synth ON synth.id = sts.synthetic_block_id
      JOIN stacks_block sb ON sb.id = synth.stacks_block_id
      LEFT JOIN contract c ON c.id = tx.contract_id
      LEFT JOIN principal p ON p.id = c.issuer_principal_id
      LEFT JOIN contract_fn cf ON cf.id = tx.contract_fn_id
      LEFT JOIN stacks_tx_type ttype ON ttype.id = tx.stacks_tx_type_id
      WHERE ${whereClause}
      ORDER BY ${sortExpr} ${sortDir}
      LIMIT :_limit OFFSET :_offset
    `;

    const rows = queryAll(db, sql, params);
    return res.json({ rows, total, offset, limit });
  } catch (err) {
    console.error("Transactions query error:", err);
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Transaction heatmap max values endpoint (for virtual scrolling)
app.get("/api/transactions/maxes", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");

    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }

    const conditions: string[] = ["sts.benchmark_run_id = :run_id"];
    const params: Record<string, unknown> = { run_id: runId };

    const filterResult = parseFilterParam(req.query.filter as string | undefined);
    if (filterResult) {
      conditions.push(filterResult.sql);
      Object.assign(params, filterResult.params);
    }

    const whereClause = conditions.join(" AND ");

    const sql = `
      SELECT
        COALESCE(MAX(sts.duration_us / 1000.0), 0) AS duration_ms,
        COALESCE(MAX(sts.clarity_runtime), 0) AS clarity_runtime,
        COALESCE(MAX(sts.clarity_read_count), 0) AS clarity_read_count,
        COALESCE(MAX(sts.clarity_read_length), 0) AS clarity_read_length,
        COALESCE(MAX(sts.clarity_write_count), 0) AS clarity_write_count,
        COALESCE(MAX(sts.clarity_write_length), 0) AS clarity_write_length
      FROM stacks_tx_stats sts
      JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
      LEFT JOIN contract c ON c.id = tx.contract_id
      LEFT JOIN principal p ON p.id = c.issuer_principal_id
      LEFT JOIN contract_fn cf ON cf.id = tx.contract_fn_id
      LEFT JOIN stacks_tx_type ttype ON ttype.id = tx.stacks_tx_type_id
      JOIN synthetic_block synth ON synth.id = sts.synthetic_block_id
      JOIN stacks_block sb ON sb.id = synth.stacks_block_id
      WHERE ${whereClause}
    `;

    const rows = queryAll(db, sql, params);
    return res.json({ maxes: rows[0] || {} });
  } catch (err) {
    console.error("Transactions maxes query error:", err);
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Autocomplete for transactions filters
// Accepts: run_id, field (one of FILTER_FIELD_MAP keys), q (search text), limit, filter (JSON DSL)
app.get("/api/transactions/autocomplete", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const field = req.query.field as string | undefined;
    const query = String(req.query.q || "").trim();
    const limit = parseOptionalInt(req.query.limit, "limit") || 30;

    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }
    if (!field || !ALLOWED_FIELDS.has(field)) {
      return res.status(400).json({ error: `field must be one of: ${[...ALLOWED_FIELDS].join(", ")}` });
    }

    const sqlField = FILTER_FIELD_MAP[field as FilterField];
    const conditions: string[] = ["sts.benchmark_run_id = :run_id"];
    const params: Record<string, unknown> = { run_id: runId, _limit: limit };

    // Apply existing filter DSL (so autocomplete narrows with other active filters)
    const filterResult = parseFilterParam(req.query.filter as string | undefined);
    if (filterResult) {
      conditions.push(filterResult.sql);
      Object.assign(params, filterResult.params);
    }

    // Apply search text
    if (query) {
      params._acq = `%${query}%`;
      conditions.push(`${sqlField} LIKE :_acq`);
    }

    // Only return non-null values
    conditions.push(`${sqlField} IS NOT NULL`);

    const sql = `
      SELECT DISTINCT ${sqlField} AS value
      FROM stacks_tx_stats sts
      JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
      LEFT JOIN contract c ON c.id = tx.contract_id
      LEFT JOIN principal p ON p.id = c.issuer_principal_id
      LEFT JOIN contract_fn cf ON cf.id = tx.contract_fn_id
      LEFT JOIN stacks_tx_type ttype ON ttype.id = tx.stacks_tx_type_id
      JOIN synthetic_block synth ON synth.id = sts.synthetic_block_id
      JOIN stacks_block sb ON sb.id = synth.stacks_block_id
      WHERE ${conditions.join(" AND ")}
      ORDER BY ${sqlField}
      LIMIT :_limit
    `;

    const rows = queryAll<{ value: string }>(db, sql, params);
    return res.json({ values: rows.map((row) => row.value) });
  } catch (err) {
    console.error("Transactions autocomplete error:", err);
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Get available contracts for filtering
app.get("/api/contracts", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }

    const rows = queryAll(
      db,
      `
        SELECT DISTINCT c.id, c.name, p.address AS issuer
        FROM stacks_tx_stats sts
        JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
        JOIN contract c ON c.id = tx.contract_id
        JOIN principal p ON p.id = c.issuer_principal_id
        WHERE sts.benchmark_run_id = :run_id
        ORDER BY c.name
        LIMIT 1000
      `,
      { run_id: runId }
    );
    return res.json(rows);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Get available transaction types (enum values for filters)
app.get("/api/tx-types", (_req: Request, res: Response) => {
  try {
    const rows = queryAll(
      db,
      `SELECT id, name FROM stacks_tx_type ORDER BY name`,
      {}
    );
    return res.json(rows);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

// Get available contract functions for filtering
app.get("/api/contract-functions", (req: Request, res: Response) => {
  try {
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const contractName = (req.query.contract as string) || null;
    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }

    let filterSql = "";
    const params: Record<string, unknown> = { run_id: runId };
    if (contractName) {
      filterSql = "AND c.name = :contract_name";
      params.contract_name = contractName;
    }

    const rows = queryAll(
      db,
      `
        SELECT DISTINCT cf.id, cf.name, c.name AS contract_name
        FROM stacks_tx_stats sts
        JOIN stacks_tx tx ON tx.id = sts.stacks_tx_id
        JOIN contract c ON c.id = tx.contract_id
        JOIN contract_fn cf ON cf.id = tx.contract_fn_id
        WHERE sts.benchmark_run_id = :run_id
          ${filterSql}
        ORDER BY cf.name
        LIMIT 1000
      `,
      params
    );
    return res.json(rows);
  } catch (err) {
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

app.get("/api/trace", (req: Request, res: Response) => {
  try {
    const mode = (req.query.mode as string) || "tx";
    const runId = parseOptionalInt(req.query.run_id, "run_id");
    const stacksTxId = parseOptionalInt(req.query.stacks_tx_id, "stacks_tx_id");
    const stacksBlockId = parseOptionalInt(req.query.stacks_block_id, "stacks_block_id");
    const segmentRootId = parseOptionalInt(req.query.segment_root_id, "segment_root_id");
    const minWallMs = parseOptionalInt(req.query.min_wall_ms, "min_wall_ms");
    const limit = parseOptionalInt(req.query.limit, "limit") || 5000;

    if (runId == null) {
      return res.status(400).json({ error: "run_id is required" });
    }

    // For tx mode, require stacks_tx_id
    if (mode === "tx" && stacksTxId == null) {
      return res.status(400).json({ error: "stacks_tx_id is required for tx mode" });
    }

    const params = {
      run_id: runId,
      stacks_tx_id: stacksTxId,
      stacks_block_id: stacksBlockId,
      segment_root_id: segmentRootId,
      min_wall_us: minWallMs == null ? null : minWallMs * 1000,
      limit,
    };

    const sql = mode === "tx" ? traceSqlTxMode() : traceSqlRunMode();
    const rows = queryAll(db, sql, params);
    
    // Return 404 if no results in tx mode (transaction not found)
    if (mode === "tx" && rows.length === 0) {
      return res.status(404).json({ error: "Transaction not found in this run" });
    }
    
    return res.json(rows);
  } catch (err) {
    console.error("Trace query error:", err);
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

app.get("/api/record/:recordId/kv", (req: Request, res: Response) => {
  try {
    const recordId = parseOptionalInt(req.params.recordId, "recordId");
    if (recordId == null) {
      return res.status(400).json({ error: "recordId is required" });
    }

    const sql = `
      SELECT
        k.key,
        v.value,
        vt.name AS value_type,
        prkv.count
      FROM profiler_record_kv prkv
      JOIN profiler_kv_key k ON k.id = prkv.profiler_kv_key_id
      JOIN profiler_kv_value v ON v.id = prkv.profiler_kv_value_id
      JOIN profiler_kv_value_type vt ON vt.id = v.profiler_kv_value_type_id
      WHERE prkv.profiler_record_id = :record_id
      ORDER BY k.key, v.value
    `;

    const rows = queryAll(db, sql, { record_id: recordId });
    return res.json(rows);
  } catch (err) {
    console.error("Record KV query error:", err);
    const error = err as Error & { status?: number };
    return res.status(error.status || 500).json({ error: error.message });
  }
});

if (fs.existsSync(distDir)) {
  app.use(express.static(distDir));
}

app.use((req: Request, res: Response) => {
  if (req.path.startsWith("/api")) {
    return res.status(404).json({ error: "Not found" });
  }
  const indexPath = path.join(distDir, "index.html");
  if (fs.existsSync(indexPath)) {
    return res.sendFile(indexPath);
  }
  return res.status(404).send("UI build not found. Run npm run build in web/.");
});

app.listen(port, host, () => {
  console.log(`Profiler Explorer listening on http://${host}:${port}`);
});
