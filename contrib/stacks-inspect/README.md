# stacks-inspect

A multifunction inspection CLI for Stacks chain data and networking.

## Build

```bash
cargo build -p stacks-inspect
```

## Global Options

```
--config <CONFIG_FILE>       Path to stacks-node configuration file
--network-config <NETWORK>   Use a predefined network (helium, mainnet, mocknet, xenon)
--version                    Show version information
--help                       Show help
```

## Commands

### Decode Commands

Decode and inspect Bitcoin/Stacks primitives.

```
decode-bitcoin-header <BLOCK_HEIGHT> <HEADERS_PATH> [-t|--testnet] [-r|--regtest]
    Decode a Bitcoin block header from SPV data

decode-tx <TX_HEX> [--file]
    Decode and print a Stacks transaction
    Use --file to read from file path (use "-" for stdin)

decode-block <BLOCK_PATH>
    Decode and print a Stacks block (epoch 2.x)
    Use "-" for stdin

decode-nakamoto-block <BLOCK_HEX> [--file]
    Decode and print a Nakamoto block
    Use --file to read from file path (use "-" for stdin)

decode-net-message <MESSAGE_DATA> [--file]
    Decode and print a Stacks network message
    Input is JSON byte array; use --file to read from file (use "-" for stdin)

decode-microblocks <MICROBLOCKS_PATH>
    Decode and print a microblock stream
    Use "-" for stdin
```

### MARF/Database Commands

Query and inspect MARF and Clarity databases.

```
header-indexed-get <BLOCK_ID_HASH> <KEY> [--state-dir <DIR> | --marf-path <PATH> --data-db-path <PATH>]
    Query header-indexed MARF data
    Provide --state-dir to auto-derive paths, or both --marf-path and --data-db-path

marf-get <MARF_PATH> <TIP_HASH> <CONSENSUS_HASH> <KEY>
    Get a value from the MARF database

deserialize-db <DB_PATH> <BYTE_PREFIX>
    Deserialize values from Clarity database filtered by byte prefix

check-deser-data <CHECK_FILE>
    Verify deserialized data from file

get-ancestors <DB_PATH> <BLOCK_HASH> <CONSENSUS_HASH>
    Trace block ancestry through staging database
```

### Shadow Block Commands

Tools for shadow chainstate operations (use with caution).

```
make-shadow-block <CHAINSTATE_DIR> <NETWORK> <CHAIN_TIP> [TX_HEX...]
    Create a shadow block from transactions
    NETWORK: mainnet, krypton, naka3

shadow-chainstate-repair <CHAINSTATE_DIR> <NETWORK>
    Repair shadow chainstate by generating and applying shadow blocks

shadow-chainstate-patch <CHAINSTATE_DIR> <NETWORK> <SHADOW_BLOCKS_JSON>
    Apply shadow blocks from JSON to chainstate
    Use "-" to read JSON from stdin

add-shadow-block <CHAINSTATE_DIR> <NETWORK> <SHADOW_BLOCK_HEX>
    Add a shadow block to chainstate
```

### Nakamoto Commands

Nakamoto-specific chain inspection and peer queries.

```
get-nakamoto-tip <CHAINSTATE_DIR> <NETWORK>
    Get the Nakamoto chain tip

get-account <CHAINSTATE_DIR> <NETWORK> <ADDRESS> [CHAIN_TIP]
    Get account state at a chain tip

getnakamotoinv <PEER_ADDR> <DATA_PORT> <CONSENSUS_HASH>
    Get Nakamoto inventory from a peer
    PEER_ADDR format: HOST:PORT
```

### Mining Commands

Mining simulation and replay tools.

```
try-mine <CHAINSTATE_PATH> [--min-fee <FEE>] [--max-time <MS>]
    Simulate mining an anchored block

tip-mine <WORKING_DIR> <EVENT_LOG> <MINE_TIP_HEIGHT> <MAX_TXNS>
    Mine a block at tip height using event log

replay-mock-mining <CHAINSTATE_PATH> <MOCK_MINING_OUTPUT_PATH>
    Replay mock-mined blocks from JSON files
```

### Validation Commands

Block validation and integrity checks.

```
validate-block <DATABASE_PATH> [--early-exit] [--ignore-costs] [MODE]
    Validate Stacks blocks (Epoch2 and Nakamoto) from chainstate database
    --early-exit: Stop on first error (default: collect all errors)
    --ignore-costs: Don't fail a block whose only mismatch is its execution cost.
        The mismatch is logged and the block counts as valid.

    MODE options:
      prefix <HASH_PREFIX>           Validate blocks matching hash prefix
      last <COUNT>                   Validate last N blocks by height
      range <START> <END>            Validate blocks in height range (inclusive)
      index-range [START] [END]      Validate Epoch2 blocks by index (omit args to show count)
      naka-index-range [START] [END] Validate Nakamoto blocks by index (omit args to show count)
```

On this optimized branch, `validate-block` first prepares the chainstate at
`DATABASE_PATH/chainstate`. A legacy Clarity database is converted to the
extent-backed side store, then to the published V4 trie format. Missing
direct-addressed MARF hash indexes and the Clarity PtrHash base are built
before validation opens the chainstate. Each conversion uses a resumable
sibling directory and only replaces the active Clarity directory after its
output is verified. Preparation needs substantial free space and an offline,
writable chainstate copy; a failed step stops validation rather than falling
back to legacy storage.

### Relocating an already converted Clarity chainstate

New PtrHash generations built beside `chainstate/vm/clarity/marf.sqlite` record
their sibling directory name, so the database and index can move together.
Older snapshots may register an absolute path. The PtrHash function files also
use native serialization and must be rebuilt when moving from another CPU
architecture. Neither case requires rewriting the migrated tries or values.

On an **offline, disposable destination snapshot** that retains the complete
`clarity_extent_index` and has an empty `clarity_extent_delta`, run:

```bash
cargo build --release -p extent-ptrhash --bin build-extent-ptrhash
python3 contrib/stacks-inspect/scripts/rebuild-portable-ptrhash.py \
  --db /path/to/snapshot/chainstate/vm/clarity/marf.sqlite \
  --builder target/release/build-extent-ptrhash \
  --expected-keys EXPECTED_HISTORICAL_KEY_COUNT \
  --audit /path/to/ptrhash-rebuild-audit.json
```

The helper checks the value generation and full historical key count before
removing the imported PtrHash registration. It then builds a new native index
beside the database and verifies the relative registration. Stop all readers
and writers first. If the delta is nonempty, the helper refuses to run; those
entries need a merge-aware rebuild. Do not run it on an original source or on
a chainstate actively used by a node.

### Chain State Commands

Chain state queries and replay.

```
get-tenure <CHAIN_STATE_DIR> <BLOCK_HASH>
    Get block tenure information

get-block-inventory <WORKING_DIR>
    Get block inventory (2100 headers)

can-download-microblock <WORKING_DIR>
    Check if microblocks can be downloaded

replay-chainstate <OLD_CHAINSTATE> <OLD_SORTITION> <OLD_BURNCHAIN> <NEW_CHAINSTATE> <NEW_BURNCHAIN>
    Replay chainstate from old to new database
```

### PoX/Sortition Commands

PoX anchor evaluation and sortition analysis.

```
evaluate-pox-anchor <SORTITION_DB_PATH> <START_HEIGHT> [END_HEIGHT]
    Evaluate PoX anchor selection at a block height
    END_HEIGHT defaults to START_HEIGHT if omitted

analyze-sortition-mev <BURNCHAIN_DB> <SORTITION_DB> <CHAINSTATE> <START> <END> [MINER BURN...]
    Analyze sortition MEV across epochs
    MINER BURN pairs specify miner advantages
```

### Utility Commands

General utilities and documentation generation.

```
peer-pub-key <LOCAL_PEER_SEED>
    Generate peer public key from seed (hex)

post-stackerdb <SLOT_ID> <SLOT_VERSION> <PRIVATE_KEY> <DATA>
    Create a signed StackerDB chunk
    DATA: raw string, file path, or "-" for stdin

contract-hash <CONTRACT_SOURCE>
    Compute contract hash from source file (use "-" for stdin)

dump-consts
    Output blockchain constants as JSON

docgen
    Generate Clarity API reference as JSON

docgen-boot
    Generate boot contracts reference as JSON

exec-program <PROGRAM_FILE>
    Execute a Clarity program file
```

## Examples

```bash
# Show version
cargo run -p stacks-inspect -- --version

# Decode a Bitcoin header from SPV file
cargo run -p stacks-inspect -- decode-bitcoin-header 800000 /path/to/headers.dat

# Decode a transaction from hex
cargo run -p stacks-inspect -- decode-tx 0x00000001...

# Decode a transaction from file
cargo run -p stacks-inspect -- decode-tx --file /path/to/tx.hex

# Get account balance at current tip
cargo run -p stacks-inspect -- get-account /path/to/chainstate mainnet SP2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKNRV9EJ7

# Validate last 100 blocks
cargo run -p stacks-inspect -- validate-block /path/to/chainstate last 100

# Analyze sortition MEV behavior
cargo run -p stacks-inspect -- analyze-sortition-mev /path/to/burnchain.db /path/to/sortition.db /path/to/chainstate 800000 800100

# Generate Clarity API docs
cargo run -p stacks-inspect -- docgen > clarity-api.json
```

## Notes

- Some commands expect mainnet data paths by default and may require specific network contexts.
- Operations that write data (e.g., shadow chain tools) are destructive—use copies of data directories when experimenting.
- Use `--help` on any command for detailed argument information: `cargo run -p stacks-inspect -- <command> --help`
