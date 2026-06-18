# Stacks Bench Profiler Explorer

Lightweight single-page app for exploring profiler traces with a true tree view.

## Setup

```bash
cd contrib/stacks-bench/tools/profiler-explorer
npm install
```

Install the UI dependencies (once):

```bash
cd contrib/stacks-bench/tools/profiler-explorer/web
npm install
```

Build the UI bundle:

```bash
npm run build
```

## Run

```bash
node server.js --db /path/to/stacks-bench.db --port 8800
```

Defaults:

- Uses `STACKS_BENCH_DB` if set
- Otherwise falls back to `./.stacks-bench/appdata/stacks-bench.db`

Then open <http://127.0.0.1:8800/>.

## UI Dev (optional)

```bash
cd contrib/stacks-bench/tools/profiler-explorer/web
npm run dev
```

This starts the Vite dev server on <http://127.0.0.1:5173/> and proxies `/api/*` to the Node backend on port 8800.

## Notes

- Transaction mode mirrors the Metabase tx trace query (ancestors + descendants).
- Run scope mode mirrors the run/block query with optional filters.
- Results are returned as flat rows and rendered as a collapsible tree client-side.
- Clarity costs are sourced from `profiler_record_clarity_costs`.
