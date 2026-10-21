# Profiler Explorer - AI Agent Guidelines

This document provides context for AI agents working with the Stacks Bench Profiler Explorer codebase.

## AI Guidelines

### Context discipline

- Do not read or paste full files unless explicitly necessary.
- When reading a file, read only the smallest relevant region (function/impl/module), ideally ≤ 200 lines.
- Prefer rg/symbol search first; open files only after narrowing to a target.
- When showing code, quote only the minimal snippet required to justify a change.

### Log discipline

- Never include full build/test logs in the conversation.
- Summarize failures in ≤ 10 bullets and include only the last ~200 lines (or less) of relevant output.

### Diff discipline

- Avoid repeating large diffs in chat. Summarize changes and reference filenames + key functions instead.

### Code Review Discipline

Use a two-pass review and only escalate when needed:

1. Pass 1: Scan the diff only. Flag high-risk issues, invariants violated, and questions. Do not open additional files unless you see a red flag.
2. Pass 2: Now open only the specific functions/modules you flagged and do a deep review.

## Overview

The Profiler Explorer is a web-based tool for visualizing and analyzing profiler traces from Stacks Bench benchmark runs. It displays hierarchical call trees with timing metrics (wall time, CPU time, wait time), Clarity VM costs, and key-value operation counts.

## Architecture

```text
profiler-explorer/
├── server.ts                 # Node/Express REST API backend (TypeScript, run via node --experimental-strip-types)
├── filter-dsl.ts             # MongoDB-style filter DSL → SQL compiler
├── filter-dsl.test.ts        # 71 backend unit tests (node --test)
├── types.ts                  # Shared backend types (FilterNode, RunRow, TraceRecord, etc.)
├── package.json              # Node dependencies/scripts (type: "module")
└── web/                      # Primary React/Vite frontend (TypeScript)
    ├── src/
    │   ├── App.tsx           # Main component (~1086 lines)
    │   ├── main.tsx          # Entry point
    │   ├── columnsConfig.ts  # Column definitions, selectable/visible helpers
    │   ├── profilerConfig.ts # Shared defaults (themes, heat colors, number formats)
    │   ├── treeTransforms.ts # Tree transforms + metric helpers
    │   ├── columnBuilders.tsx # Header builder helpers
    │   ├── styles.css        # Tailwind + custom CSS
    │   ├── contexts/         # React context providers
    │   │   ├── ProfilerGridContext.tsx
    │   │   └── TransactionsGridContext.tsx
    │   ├── components/       # Extracted UI components (.tsx)
    │   └── ui/               # UI primitives (.tsx)
    ├── tsconfig.json         # strict: true, paths: @/* → ./src/*
    ├── package.json
    ├── vite.config.js
    └── tailwind.config.js
```

### Technology Stack

- **Backend**: Node.js 24+ (native TypeScript via `--experimental-strip-types`), Express 5, better-sqlite3
- **Frontend**: React 19, Vite 7, Tailwind CSS 4, @base-ui/react, @svar-ui/react-grid
- **Type checking**: TypeScript 5.9 (strict mode, `tsc --noEmit` for frontend)
- **Database**: SQLite (located at `~/.stacks-bench/appdata/stacks-bench.db` or via `STACKS_BENCH_DB` env var)

## Backend API (`server.ts`)

### Endpoints

| Endpoint | Purpose |
| ---------- | --------- |
| `GET /api/runs` | List benchmark runs (latest 200) |
| `GET /api/blocks?run_id=N` | List blocks in a run |
| `GET /api/txs?run_id=N&q=prefix` | List/search transactions |
| `GET /api/tx-lookup?run_id=N&tx_hash=...` | Lookup transaction by hash |
| `GET /api/trace` | **Main endpoint** - returns profiler trace tree |

### Trace Query Modes

1. **TX Mode** (`mode=tx`): Transaction-focused trace showing all spans related to a transaction execution
   - Parameters: `run_id`, `stacks_tx_id`, `limit`

2. **RUN Mode** (`mode=run`): Block/segment scope showing span tree within specific boundaries
   - Parameters: `run_id`, `stacks_block_id`, `segment_root_id`, `min_wall_us`, `limit`

### SQL Query Patterns

The backend uses recursive CTEs to build ancestor/descendant trees:

- Walk up parent chain from seed nodes
- Walk down child chain from seed nodes
- Union both for complete trace
- Join with profiler_span, profiler_tag, stacks_tx, contract, principal tables
- Aggregate KV operations and Clarity costs

### Key Tables

- `benchmark_run` - Benchmark execution metadata
- `profiler_record` - Core trace span records
- `profiler_span` - Span definitions (name, context)
- `profiler_tag` - Optional categorization tags
- `stacks_block`, `stacks_tx` - Blockchain data
- `contract`, `contract_fn`, `principal` - Smart contract info
- `profiler_record_kv` - Key-value store operations
- `profiler_record_clarity_costs` - Clarity VM cost tracking

## Frontend (`web/src/App.tsx`)

### Frontend Module Map

- `columnsConfig.ts`: column definitions (`ColumnDef`), selectable/visible helpers, `NUMERIC_COLUMN_KEYS`
- `profilerConfig.ts`: shared defaults (themes, heat colors, number formats, auto-expand)
- `treeTransforms.ts`: tree transforms (`TreeNode` type) + metric helpers
- `columnBuilders.tsx`: header builder helpers (`createHeatHeaderBuilder`, `createSpanHeaderBuilder`)
- `contexts/ProfilerGridContext.tsx`: profiler grid context (`ProfilerGridCallbacks`, `HeatConfigEntry`, `SpanVizConfig`)
- `contexts/TransactionsGridContext.tsx`: transactions grid context (`TransactionsGridCallbacks`)
- `components/filter-builder.tsx`: MongoDB-style filter DSL UI (`FilterBuilder`, `FilterRule`, `FilterFieldDef`)
- `components/`: extracted UI pieces (`HeaderBar`, `ToolbarBar`, `BreadcrumbBar`, `HeatCells`,
  `SpanCell`, `HeatHeaderCell`, `SpanHeaderCell`, `SettingsPanel`, `ProfilerGrid`, `TransactionsTab`, `SpanDetailsModal`)
- `ui/`: UI primitives (button, input, select, popover, dialog, tooltip, etc.)

### State Structure

```javascript
// Data selection
runs, blocks, txs, runId, stacksTxId, stacksBlockId

// Query parameters
mode ("tx" | "run"), minWallMs, segmentRootId, limit

// Display configuration
selectedColumns, numberFormatId, chainCompression, hotPathMode
heatConfig (per-metric heat map settings)

// Tree navigation
focusId, activeId, openNodes, expandedChains

// Loaded data
rows (records array), summary (status text)
```

### Data Transformation Pipeline

1. `buildTreeIndex()` - Flat records to parent-child tree
2. `pruneTree()` - Filter nodes below minWallMs threshold
3. `applyFocus()` - Zoom into selected node as root
4. `applyHotPath()` - Show only highest-cost branch
5. `applyChainCompression()` - Collapse single-child linear chains
6. `applyOpenState()` - Track expanded/collapsed state
7. Compute flame percentages for visualization

### Column System

18 columns defined in `COLUMN_DEFS`:

- Span (tree toggle + flame bar + name)
- Calls, Wall Time (Total/Avg), Busy Time (Total/Avg), Wait Time (Total)
- Samples, KV Operations
- Clarity metrics (Runtime, Read/Write counts and lengths)

Each column has: `key`, `label`, `width`, `default`, `getter`, `format`, `heatKey`, `render`

### Heat Map System

Per-metric configuration with:

- Enable/disable toggle
- Auto-calculated or custom min/max bounds
- Alpha gradient (5-27% opacity)

Heat keys: `wallTotalUs`, `selfWallUs`, `busyTotalUs`, `selfBusyUs`, `waitTotalUs`, `selfWaitUs`, `clarityRuntime`

### localStorage Keys

- `profilerColumns` - Selected visible columns
- `profilerNumberFormat` - Number format preference
- `profilerHeatConfig` - Heat map settings

## Development

### Running Locally

```bash
# Backend
cd contrib/stacks-bench/tools/profiler-explorer
npm install
node --experimental-strip-types server.ts  # Starts on port 8800

# Backend tests
node --test filter-dsl.test.ts  # 71 tests

# Frontend (development)
cd web
npm install
npm run dev  # Starts on port 5173 with API proxy

# Frontend type checking
npx tsc --noEmit  # Strict mode, zero errors expected

# Frontend (production build)
npm run build  # Outputs to dist/
```

### Environment Variables

- `STACKS_BENCH_DB` - Override database path (default: `~/.stacks-bench/appdata/stacks-bench.db`)
- `PORT` - Override backend port (default: 8800)

## Common Tasks for Agents

### Adding a New Column

1. Add entry to `COLUMN_DEFS` array in `columnsConfig.ts`
2. Define `key`, `label`, `width`, `getter` function
3. Optionally add `heatKey` for heat map support
4. Add custom `cell` function if needed

### Adding a New API Endpoint

1. Add route in `server.ts` with `app.get()`
2. Use parameterized SQL queries (never string interpolation)
3. Add types to `types.ts` for request/response shapes
4. Return JSON with appropriate error handling

### Modifying Tree Transformation

Tree transforms are in the pipeline between data fetch and render. Each function takes a tree and returns a modified tree. Maintain the node structure: `{ id, parentId, children, ...data }`.

### Adding Heat Map Support

1. Add `heatKey` to column definition
2. Ensure getter returns numeric microseconds
3. Heat config auto-initializes on first load

## Code Style Notes

- All code is TypeScript (strict mode for frontend, `--experimental-strip-types` for backend)
- React functional components with hooks
- Tailwind CSS utility classes for styling
- @base-ui/react for accessible UI primitives
- Custom CSS variables use oklch color space
- Dark theme is the default/only theme
- Number formatting supports international locales
- Types are co-located: `types.ts` (backend), context files (frontend shared types), component files (component-local types)
