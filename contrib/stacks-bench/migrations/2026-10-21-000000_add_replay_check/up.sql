-- Correctness records of every replayed segment of a benchmark run: warmup
-- entries, unmeasured prefix segments and every repetition included. Each row
-- holds the state root the segment's synthetic block committed and a digest
-- of its transaction receipts (see `stacks_bench::replay::receipts_digest`).
-- `bench verify` compares two runs row by row and rejects the comparison on
-- any mismatch; `canonical_root_match` records the check against the
-- canonical header root where that comparison is defined (single full-block
-- segment at repetition 0).
CREATE TABLE replay_check (
  benchmark_run_id INTEGER NOT NULL,
  origin_index_hash BLOB NOT NULL,
  segment_index INTEGER NOT NULL,
  tx_start INTEGER NOT NULL,
  tx_end INTEGER NOT NULL,
  repetition INTEGER NOT NULL,
  is_warmup BOOLEAN NOT NULL,
  measured BOOLEAN NOT NULL,
  state_index_root BLOB NOT NULL,
  receipts_digest BLOB NOT NULL,
  canonical_root_match BOOLEAN,

  PRIMARY KEY (benchmark_run_id, origin_index_hash, segment_index, tx_start, tx_end, repetition),
  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id) ON DELETE CASCADE,
  CHECK(length(origin_index_hash) = 32),
  CHECK(length(state_index_root) = 32),
  CHECK(length(receipts_digest) = 32)
);
