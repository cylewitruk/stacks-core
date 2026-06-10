# Claude Code Guidelines

See [AGENTS.md](./AGENTS.md) for comprehensive documentation on this codebase.

## Quick Reference

**What is this?** A web-based profiler trace explorer for Stacks Bench benchmark data.

**Stack:** Node/Express backend + React/Vite frontend + SQLite database

**Key files:**

- [server.js](./server.js) - REST API with recursive CTE queries for trace tree building
- [web/src/App.jsx](./web/src/App.jsx) - Main React component with tree transformations
- [static/app.js](./static/app.js) - Vanilla JS fallback UI

**Run locally:**

```bash
node server.js       # Backend on :8800
cd web && npm run dev  # Frontend on :5173
```

**Database:** `~/.stacks-bench/appdata/stacks-bench.db` (override with `STACKS_BENCH_DB` env var)
