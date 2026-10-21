// ---------------------------------------------------------------------------
// Tests for filter-dsl.ts — MongoDB-style filter DSL → parameterized SQL
// ---------------------------------------------------------------------------
// Run with: node --test filter-dsl.test.ts

import { describe, it, beforeEach } from "node:test";
import assert from "node:assert/strict";
import {
  buildWhere,
  parseFilterParam,
  resetParamCounter,
  FILTER_FIELD_MAP,
  ALLOWED_FIELDS,
  ALLOWED_OPS,
} from "./filter-dsl.ts";

// Reset param counter before every test for deterministic param names
beforeEach(() => resetParamCounter());

// ═══════════════════════════════════════════════════════════════════════════
// buildWhere – leaf nodes
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – leaf nodes", () => {
  it("simple $eq", () => {
    const { sql, params } = buildWhere({ contract_name: { $eq: "bns" } });
    assert.equal(sql, "c.name = :_fp0");
    assert.deepEqual(params, { _fp0: "bns" });
  });

  it("$ne", () => {
    const { sql, params } = buildWhere({ contract_name: { $ne: "bns" } });
    assert.equal(sql, "c.name != :_fp0");
    assert.deepEqual(params, { _fp0: "bns" });
  });

  it("$gt on numeric field", () => {
    const { sql, params } = buildWhere({ duration_ms: { $gt: 100 } });
    assert.equal(sql, "(sts.duration_us / 1000.0) > :_fp0");
    assert.deepEqual(params, { _fp0: 100 });
  });

  it("$gte", () => {
    const { sql, params } = buildWhere({ duration_ms: { $gte: 50 } });
    assert.equal(sql, "(sts.duration_us / 1000.0) >= :_fp0");
    assert.deepEqual(params, { _fp0: 50 });
  });

  it("$lt", () => {
    const { sql, params } = buildWhere({ clarity_runtime: { $lt: 200 } });
    assert.equal(sql, "sts.clarity_runtime < :_fp0");
    assert.deepEqual(params, { _fp0: 200 });
  });

  it("$lte", () => {
    const { sql, params } = buildWhere({ clarity_runtime: { $lte: 999 } });
    assert.equal(sql, "sts.clarity_runtime <= :_fp0");
    assert.deepEqual(params, { _fp0: 999 });
  });

  it("$contains wraps value with % wildcards", () => {
    const { sql, params } = buildWhere({ contract_fn: { $contains: "mint" } });
    assert.equal(sql, "cf.name LIKE :_fp0");
    assert.deepEqual(params, { _fp0: "%mint%" });
  });

  it("$ncontains wraps value with % wildcards using NOT LIKE", () => {
    const { sql, params } = buildWhere({ contract_fn: { $ncontains: "transfer" } });
    assert.equal(sql, "cf.name NOT LIKE :_fp0");
    assert.deepEqual(params, { _fp0: "%transfer%" });
  });

  it("$startsWith appends % suffix", () => {
    const { sql, params } = buildWhere({ contract_name: { $startsWith: "bns" } });
    assert.equal(sql, "c.name LIKE :_fp0");
    assert.deepEqual(params, { _fp0: "bns%" });
  });

  it("$endsWith prepends % prefix", () => {
    const { sql, params } = buildWhere({ contract_name: { $endsWith: "token" } });
    assert.equal(sql, "c.name LIKE :_fp0");
    assert.deepEqual(params, { _fp0: "%token" });
  });

  it("every allowed field maps to its SQL expression", () => {
    for (const field of ALLOWED_FIELDS) {
      resetParamCounter();
      const { sql } = buildWhere({ [field]: { $eq: "x" } });
      assert.ok(
        sql.startsWith(FILTER_FIELD_MAP[field]),
        `Expected sql to start with "${FILTER_FIELD_MAP[field]}", got "${sql}"`,
      );
    }
  });

  it("every allowed operator is accepted", () => {
    for (const op of ALLOWED_OPS) {
      resetParamCounter();
      // $in/$nin requires an array value; all others take a scalar
      const val = (op === "$in" || op === "$nin") ? ["a", "b"] : "x";
      const { sql } = buildWhere({ contract_name: { [op]: val } });
      assert.ok(sql.includes(":_fp"), `op ${op} should produce a param`);
    }
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// buildWhere – combinators ($and / $or)
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – combinators", () => {
  it("$and with two leaves", () => {
    const { sql, params } = buildWhere({
      $and: [
        { contract_name: { $contains: "foo" } },
        { duration_ms: { $gte: 100 } },
      ],
    });
    assert.equal(sql, "(c.name LIKE :_fp0 AND (sts.duration_us / 1000.0) >= :_fp1)");
    assert.deepEqual(params, { _fp0: "%foo%", _fp1: 100 });
  });

  it("$or with two leaves", () => {
    const { sql, params } = buildWhere({
      $or: [
        { contract_issuer: { $eq: "SP123" } },
        { contract_issuer: { $eq: "SP456" } },
      ],
    });
    assert.equal(sql, "(p.address = :_fp0 OR p.address = :_fp1)");
    assert.deepEqual(params, { _fp0: "SP123", _fp1: "SP456" });
  });

  it("nested $and inside $or", () => {
    const { sql, params } = buildWhere({
      $or: [
        {
          $and: [
            { contract_name: { $eq: "bns" } },
            { contract_fn: { $contains: "mint" } },
          ],
        },
        { duration_ms: { $gt: 500 } },
      ],
    });
    assert.equal(
      sql,
      "((c.name = :_fp0 AND cf.name LIKE :_fp1) OR (sts.duration_us / 1000.0) > :_fp2)",
    );
    assert.deepEqual(params, { _fp0: "bns", _fp1: "%mint%", _fp2: 500 });
  });

  it("deeply nested (3 levels)", () => {
    const { sql } = buildWhere({
      $and: [
        {
          $or: [
            { $and: [{ contract_name: { $eq: "a" } }, { contract_fn: { $eq: "b" } }] },
            { contract_issuer: { $eq: "SP1" } },
          ],
        },
        { duration_ms: { $gte: 10 } },
      ],
    });
    // Should contain all AND/OR combos without error
    assert.ok(sql.includes("AND"));
    assert.ok(sql.includes("OR"));
  });

  it("empty $and returns tautology 1=1", () => {
    const { sql, params } = buildWhere({ $and: [] });
    assert.equal(sql, "1=1");
    assert.deepEqual(params, {});
  });

  it("empty $or returns tautology 1=1", () => {
    const { sql, params } = buildWhere({ $or: [] });
    assert.equal(sql, "1=1");
    assert.deepEqual(params, {});
  });

  it("single-element $and", () => {
    const { sql } = buildWhere({ $and: [{ contract_name: { $eq: "x" } }] });
    assert.equal(sql, "(c.name = :_fp0)");
  });

  it("single-element $or", () => {
    const { sql } = buildWhere({ $or: [{ contract_fn: { $contains: "y" } }] });
    assert.equal(sql, "(cf.name LIKE :_fp0)");
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// buildWhere – param uniqueness
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – param uniqueness", () => {
  it("multiple leaves get distinct param names", () => {
    const { params } = buildWhere({
      $and: [
        { contract_name: { $eq: "a" } },
        { contract_name: { $eq: "b" } },
        { contract_name: { $eq: "c" } },
      ],
    });
    const keys = Object.keys(params);
    assert.equal(keys.length, 3);
    assert.equal(new Set(keys).size, 3, "all param names should be unique");
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// buildWhere – validation / rejection
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – validation", () => {
  it("rejects null", () => {
    assert.throws(() => buildWhere(null), /Invalid filter node/);
  });

  it("rejects string", () => {
    assert.throws(() => buildWhere("bad"), /Invalid filter node/);
  });

  it("rejects number", () => {
    assert.throws(() => buildWhere(42), /Invalid filter node/);
  });

  it("rejects undefined", () => {
    assert.throws(() => buildWhere(undefined), /Invalid filter node/);
  });

  it("rejects unknown field", () => {
    assert.throws(
      () => buildWhere({ unknown_field: { $eq: "x" } }),
      /Invalid filter field: unknown_field/,
    );
  });

  it("rejects unknown operator", () => {
    assert.throws(
      () => buildWhere({ contract_name: { $regex: "x" } }),
      /Invalid filter operator: \$regex/,
    );
  });

  it("rejects leaf with multiple fields", () => {
    assert.throws(
      () => buildWhere({ contract_name: { $eq: "x" }, contract_fn: { $eq: "y" } }),
      /Filter leaf must have exactly one field/,
    );
  });

  it("rejects leaf with multiple operators", () => {
    assert.throws(
      () => buildWhere({ contract_name: { $eq: "x", $ne: "y" } }),
      /Filter leaf must have exactly one operator/,
    );
  });

  it("rejected errors have status 400", () => {
    try {
      buildWhere({ bad_field: { $eq: 1 } });
      assert.fail("should have thrown");
    } catch (err) {
      assert.equal(err.status, 400);
    }
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// SQL injection mitigation
// ═══════════════════════════════════════════════════════════════════════════

describe("SQL injection mitigation", () => {
  it("field names are allow-listed, not interpolated from user input", () => {
    // Attempt to use a SQL-injection-style field name
    assert.throws(
      () => buildWhere({ "1=1; DROP TABLE users; --": { $eq: "x" } }),
      /Invalid filter field/,
    );
  });

  it("operator names are allow-listed, not interpolated from user input", () => {
    assert.throws(
      () => buildWhere({ contract_name: { "1=1; --": "x" } }),
      /Invalid filter operator/,
    );
  });

  it("values are always parameterized, never interpolated into SQL", () => {
    const malicious = "'; DROP TABLE benchmark_run; --";
    const { sql, params } = buildWhere({ contract_name: { $eq: malicious } });
    // The SQL must NOT contain the malicious string — only a named param
    assert.ok(!sql.includes(malicious), "malicious value must not appear in SQL");
    assert.ok(sql.includes(":_fp0"), "value must be in a named param");
    assert.equal(params._fp0, malicious, "malicious string is safely in params");
  });

  it("$contains values with injection attempt are parameterized", () => {
    const malicious = "'; DELETE FROM contract; --";
    const { sql, params } = buildWhere({ contract_name: { $contains: malicious } });
    assert.ok(!sql.includes(malicious));
    assert.equal(params._fp0, `%${malicious}%`);
  });

  it("numeric field with string value is parameterized (type not coerced in SQL)", () => {
    const { sql, params } = buildWhere({ duration_ms: { $gt: "100 OR 1=1" } });
    assert.ok(!sql.includes("100 OR 1=1"));
    assert.equal(params._fp0, "100 OR 1=1");
  });

  it("field names containing SQL keywords are rejected", () => {
    assert.throws(
      () => buildWhere({ "contract_name UNION SELECT": { $eq: "x" } }),
      /Invalid filter field/,
    );
  });

  it("operator containing SQL comment sequence is rejected", () => {
    assert.throws(
      () => buildWhere({ contract_name: { "$eq --": "x" } }),
      /Invalid filter operator/,
    );
  });

  it("extremely long value is parameterized, not interpolated", () => {
    const longVal = "A".repeat(100_000);
    const { sql, params } = buildWhere({ contract_name: { $eq: longVal } });
    assert.ok(!sql.includes(longVal));
    assert.equal(params._fp0, longVal);
  });

  it("nested injection attempt in $and is parameterized", () => {
    const { sql, params } = buildWhere({
      $and: [
        { contract_name: { $eq: "x' OR '1'='1" } },
        { contract_fn: { $contains: "y' UNION SELECT * FROM users --" } },
      ],
    });
    assert.ok(!sql.includes("UNION"));
    assert.ok(!sql.includes("'1'='1"));
    assert.equal(params._fp0, "x' OR '1'='1");
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// parseFilterParam
// ═══════════════════════════════════════════════════════════════════════════

describe("parseFilterParam", () => {
  it("returns null for empty / falsy input", () => {
    assert.equal(parseFilterParam(null), null);
    assert.equal(parseFilterParam(undefined), null);
    assert.equal(parseFilterParam(""), null);
  });

  it("parses a valid JSON filter string", () => {
    const json = JSON.stringify({ contract_name: { $eq: "bns" } });
    const result = parseFilterParam(json);
    assert.ok(result);
    assert.ok(result.sql.includes("c.name"));
    assert.ok(Object.keys(result.params).length > 0);
  });

  it("rejects invalid JSON", () => {
    assert.throws(() => parseFilterParam("{bad json}"), /Invalid filter JSON/);
  });

  it("rejects truncated JSON", () => {
    assert.throws(() => parseFilterParam('{"contract_name":'), /Invalid filter JSON/);
  });

  it("enforces max depth of 6", () => {
    // Build a 7-deep nested filter
    let filter = { contract_name: { $eq: "x" } };
    for (let i = 0; i < 7; i++) {
      filter = { $and: [filter] };
    }
    assert.throws(
      () => parseFilterParam(JSON.stringify(filter)),
      /Filter too deeply nested/,
    );
  });

  it("allows depth up to 6", () => {
    // Build a 6-deep nested filter (should be fine)
    let filter = { contract_name: { $eq: "x" } };
    for (let i = 0; i < 6; i++) {
      filter = { $and: [filter] };
    }
    const result = parseFilterParam(JSON.stringify(filter));
    assert.ok(result);
    assert.ok(result.sql.length > 0);
  });

  it("depth guard error has status 400", () => {
    let filter = { contract_name: { $eq: "x" } };
    for (let i = 0; i < 7; i++) filter = { $and: [filter] };
    try {
      parseFilterParam(JSON.stringify(filter));
      assert.fail("should have thrown");
    } catch (err) {
      assert.equal(err.status, 400);
    }
  });

  it("passes through complex mixed $and/$or tree", () => {
    const filter = {
      $and: [
        {
          $or: [
            { contract_name: { $contains: "bns" } },
            { contract_fn: { $startsWith: "mint" } },
          ],
        },
        { duration_ms: { $gte: 50 } },
        { clarity_runtime: { $lt: 10000 } },
      ],
    };
    const result = parseFilterParam(JSON.stringify(filter));
    assert.ok(result.sql.includes("AND"));
    assert.ok(result.sql.includes("OR"));
    assert.equal(Object.keys(result.params).length, 4);
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// Edge cases & real-world query patterns
// ═══════════════════════════════════════════════════════════════════════════

describe("real-world query patterns", () => {
  it("single field filter (typical first filter)", () => {
    const { sql, params } = buildWhere({ contract_issuer: { $contains: "SP2" } });
    assert.equal(sql, "p.address LIKE :_fp0");
    assert.deepEqual(params, { _fp0: "%SP2%" });
  });

  it("multi-field AND (typical compound filter)", () => {
    const { sql, params } = buildWhere({
      $and: [
        { contract_issuer: { $contains: "SP2" } },
        { contract_name: { $eq: "bns" } },
        { contract_fn: { $startsWith: "name-" } },
        { duration_ms: { $gte: 10 } },
      ],
    });
    assert.ok(sql.startsWith("("));
    assert.ok(sql.includes("AND"));
    assert.equal(Object.keys(params).length, 4);
  });

  it("OR across issuers (multi-principal search)", () => {
    const { sql } = buildWhere({
      $or: [
        { contract_issuer: { $eq: "SP1AAA" } },
        { contract_issuer: { $eq: "SP2BBB" } },
        { contract_issuer: { $eq: "SP3CCC" } },
      ],
    });
    assert.equal((sql.match(/OR/g) || []).length, 2);
  });

  it("grouped OR inside a broader AND (the group feature)", () => {
    const { sql, params } = buildWhere({
      $and: [
        {
          $or: [
            { contract_name: { $eq: "bns" } },
            { contract_name: { $eq: "pox-4" } },
          ],
        },
        { duration_ms: { $gte: 100 } },
      ],
    });
    // Should produce: ((c.name = ? OR c.name = ?) AND (duration >= ?))
    assert.ok(sql.includes("OR"));
    assert.ok(sql.includes("AND"));
    assert.equal(Object.keys(params).length, 3);
  });

  it("tx_hash_hex exact match", () => {
    const hash = "abc123def456";
    const { sql, params } = buildWhere({ tx_hash_hex: { $eq: hash } });
    assert.equal(sql, "tx.tx_hash_hex = :_fp0");
    assert.equal(params._fp0, hash);
  });

  it("stacks_block_height range", () => {
    const { sql, params } = buildWhere({
      $and: [
        { stacks_block_height: { $gte: 100000 } },
        { stacks_block_height: { $lte: 200000 } },
      ],
    });
    assert.ok(sql.includes(">="));
    assert.ok(sql.includes("<="));
    assert.deepEqual(params, { _fp0: 100000, _fp1: 200000 });
  });

  it("all clarity metric fields work", () => {
    const clarityFields = [
      "clarity_runtime",
      "clarity_read_count",
      "clarity_read_length",
      "clarity_write_count",
      "clarity_write_length",
    ];
    for (const field of clarityFields) {
      resetParamCounter();
      const { sql, params } = buildWhere({ [field]: { $gt: 0 } });
      assert.ok(sql.includes(">"), `${field} should produce > clause`);
      assert.equal(params._fp0, 0);
    }
  });

  it("value with special characters is safely parameterized", () => {
    const specialChars = `hello "world" 'it\\'s' \t\n\0 ${"`"}back${"`"}`;
    const { sql, params } = buildWhere({ contract_name: { $eq: specialChars } });
    assert.ok(!sql.includes(specialChars));
    assert.equal(params._fp0, specialChars);
  });

  it("value with unicode is parameterized", () => {
    const unicode = "合约名称 🦊";
    const { sql, params } = buildWhere({ contract_name: { $contains: unicode } });
    assert.ok(!sql.includes(unicode));
    assert.ok(params._fp0.includes(unicode));
    assert.equal(params._fp0, `%${unicode}%`);
  });

  it("boolean-ish value is parameterized as-is", () => {
    const { params } = buildWhere({ contract_name: { $eq: true } });
    assert.strictEqual(params._fp0, true);
  });

  it("zero value is accepted (not treated as falsy)", () => {
    const { params } = buildWhere({ duration_ms: { $gte: 0 } });
    assert.strictEqual(params._fp0, 0);
  });

  it("negative number value is accepted", () => {
    const { params } = buildWhere({ duration_ms: { $gt: -1 } });
    assert.strictEqual(params._fp0, -1);
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// buildWhere – $in operator (multi-select)
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – $in operator", () => {
  beforeEach(() => resetParamCounter());

  it("generates IN clause with multiple params", () => {
    const { sql, params } = buildWhere({
      contract_name: { $in: ["alpha", "beta", "gamma"] },
    });
    assert.ok(sql.includes("IN"));
    assert.ok(sql.includes(":_fp1"));
    assert.ok(sql.includes(":_fp2"));
    assert.ok(sql.includes(":_fp3"));
    assert.equal(params._fp1, "alpha");
    assert.equal(params._fp2, "beta");
    assert.equal(params._fp3, "gamma");
  });

  it("single-element $in works", () => {
    const { sql, params } = buildWhere({
      contract_issuer: { $in: ["SP123"] },
    });
    assert.ok(sql.includes("IN (:_fp1)"));
    assert.equal(params._fp1, "SP123");
  });

  it("$in with empty array throws 400", () => {
    assert.throws(
      () => buildWhere({ contract_name: { $in: [] } }),
      (err) => err.status === 400 && err.message.includes("non-empty"),
    );
  });

  it("$in with non-array value throws 400", () => {
    assert.throws(
      () => buildWhere({ contract_name: { $in: "string" } }),
      (err) => err.status === 400,
    );
  });

  it("$in values are parameterized, not interpolated", () => {
    const { sql, params } = buildWhere({
      contract_name: { $in: ["'; DROP TABLE--", "normal"] },
    });
    assert.ok(!sql.includes("DROP"));
    assert.equal(Object.values(params)[0], "'; DROP TABLE--");
  });

  it("$in works inside $and combinator", () => {
    const { sql, params } = buildWhere({
      $and: [
        { contract_name: { $in: ["a", "b"] } },
        { duration_ms: { $gt: 50 } },
      ],
    });
    assert.ok(sql.includes("IN"));
    assert.ok(sql.includes(">"));
    assert.ok(Object.keys(params).length >= 3);
  });
});

// ═══════════════════════════════════════════════════════════════════════════
// $nin operator (NOT IN)
// ═══════════════════════════════════════════════════════════════════════════

describe("buildWhere – $nin operator", () => {
  beforeEach(() => resetParamCounter());

  it("generates NOT IN clause with multiple params", () => {
    const { sql, params } = buildWhere({
      tx_type_name: { $nin: ["Token Transfer", "Coinbase"] },
    });
    assert.ok(sql.includes("NOT IN"));
    assert.ok(sql.includes(":_fp1"));
    assert.ok(sql.includes(":_fp2"));
    assert.equal(params._fp1, "Token Transfer");
    assert.equal(params._fp2, "Coinbase");
  });

  it("single-element $nin works", () => {
    const { sql, params } = buildWhere({
      tx_type_name: { $nin: ["Token Transfer"] },
    });
    assert.ok(sql.includes("NOT IN (:_fp1)"));
    assert.equal(params._fp1, "Token Transfer");
  });

  it("$nin with empty array throws 400", () => {
    assert.throws(
      () => buildWhere({ tx_type_name: { $nin: [] } }),
      (err) => err.status === 400 && err.message.includes("non-empty"),
    );
  });

  it("$nin with non-array value throws 400", () => {
    assert.throws(
      () => buildWhere({ tx_type_name: { $nin: "string" } }),
      (err) => err.status === 400,
    );
  });

  it("$nin values are parameterized, not interpolated", () => {
    const { sql, params } = buildWhere({
      tx_type_name: { $nin: ["'; DROP TABLE--", "normal"] },
    });
    assert.ok(!sql.includes("DROP"));
    assert.equal(Object.values(params)[0], "'; DROP TABLE--");
  });

  it("$nin works inside $or combinator", () => {
    const { sql, params } = buildWhere({
      $or: [
        { tx_type_name: { $nin: ["a", "b"] } },
        { duration_ms: { $lt: 100 } },
      ],
    });
    assert.ok(sql.includes("NOT IN"));
    assert.ok(sql.includes("<"));
    assert.ok(Object.keys(params).length >= 3);
  });
});
