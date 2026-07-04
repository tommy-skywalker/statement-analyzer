# Statement Analyzer

A **fast, deterministic** bank-statement analysis engine — built in **Rust** (Axum) with a
plain HTML/CSS/vanilla-JS frontend. Upload a statement in almost any format, search by a
name or keyword, and get instant debit/credit intelligence: currency, totals, counts,
date ranges and activity duration.

No AI. No OCR (unless you wire it in). No frameworks. Just a single static binary that can
chew through large files in a fraction of a second.

> Built as an enterprise demo: the same engine scales from a 600-byte CSV to multi-hundred-MB
> exports without changing a line.

---

## Highlights

- **Multi-format ingestion** — one upload endpoint handles them all:
  - `CSV` / `TSV` (streaming reader)
  - `XLSX` / `XLS` / `XLSM` / `ODS` (every sheet)
  - `PDF` (text extraction via `pdf-extract`)
  - `TXT` / `LOG` / `MD` (line scanning)
  - `ZIP` (pure-Rust, recursive)
  - `RAR` / `7z` / `TAR(.gz/.bz2)` (delegated to a system `7z`/`7zz`/`unar`/`unrar`/`tar` if installed)
  - **Any other file** — falls back to a generic text scan if it decodes as text
- **Automatic currency detection** — ₦ NGN, $ USD, £ GBP, € EUR, ¥ JPY, ₹ INR, ₵ GHS, R ZAR,
  KSh KES, ₿ BTC, C$, A$, CHF, ₩ KRW, AED … with a confidence score.
- **Smart debit/credit classification**
  - Dedicated `Debit`/`Credit` (or `Withdrawal`/`Deposit`, `Money in`/`Money out`) columns
  - Single signed `Amount` column (negatives = debit) + optional `Dr/Cr` type column
  - Free-text/PDF lines via sign, parentheses `(1,000.00)`, trailing `DR/CR`, and keywords
  - Robust amount parsing: `1,234.56`, `1.234,56`, `₦1,000`, `(500)`, unicode minus
  - Transaction-vs-running-balance disambiguation on text lines
- **Per-side statistics** for both debit and credit:
  total, count, average, largest, first date, last date, and a human duration
  (`2 months 5 days`, `1 year 3 months`, …)
- **Clean structured JSON** with a summary block (net flow, overall range/duration, timing,
  throughput in MB/s).
- **Fast & deterministic** — CPU-bound work runs off the async runtime; identical input
  always yields identical output. Sub-millisecond on small statements.

---

## Project structure

```
statement-analyzer/
├── Cargo.toml
├── README.md
├── .claude/launch.json          # optional: Claude Code preview config
├── samples/
│   └── sample_statement.csv     # example statement to try
├── static/
│   └── index.html               # single-page UI (embedded CSS + JS)
└── src/
    ├── main.rs                  # Axum server + routes
    ├── model.rs                 # transaction model + JSON output schema
    ├── currency.rs              # deterministic currency detection
    ├── util.rs                  # money & date parsing helpers
    ├── extract.rs               # file-type dispatch -> normalised blocks
    ├── engine.rs                # the analysis engine (classify + stats)
    └── parsers/
        ├── mod.rs
        └── archive.rs           # ZIP (pure Rust) + RAR/7z/tar (external)
```

---

## Running locally

### Prerequisites
- [Rust](https://rustup.rs) (stable, 1.80+). That's it.
- *(Optional)* a `7z` / `7zz` / `unar` / `unrar` / `tar` binary on `PATH` to enable
  RAR/7z/tar archive support.

### Build & run
```bash
cd ~/Documents/statement-analyzer

# Build the optimized binary
cargo build --release

# Run it (defaults to port 8000; override with PORT)
./target/release/statement-analyzer
#   ▸ Statement Analyzer running at http://localhost:8000

# or, for development:
cargo run
```

Open **http://localhost:8000**, drop in a statement, type a name/keyword, hit **Analyze**.

Change the port:
```bash
PORT=9000 ./target/release/statement-analyzer
```

---

## Deploying — Vercel (frontend) + Railway (API)

This is a long-running Rust server, so the **API runs on Railway** (or any host that runs a
binary/Docker), and the **static page is hosted on Vercel**. CORS is already permissive, so
the Vercel page can call the Railway API cross-origin.

> ⚠️ The API cannot run on Vercel itself — Vercel serverless functions cap request bodies at
> ~4.5 MB and time out in seconds, which defeats the "scan big files" goal. Railway runs the
> actual binary with no such limits.

### 1) Deploy the API on Railway
1. Push this repo to GitHub (already done).
2. Railway → **New Project → Deploy from GitHub repo** → pick this repo.
3. Railway auto-detects the [`Dockerfile`](Dockerfile) (config in [`railway.json`](railway.json));
   it builds and starts the server. `PORT` is injected automatically.
4. Under **Settings → Networking → Generate Domain** to get a public URL, e.g.
   `https://statement-analyzer-production.up.railway.app`.
5. Verify: open `https://<your-railway-domain>/health` → `{"status":"ok",...}`.

The Docker image includes `p7zip-full` + `unar`, so RAR/7z/tar archive uploads work in prod.

### 2) Deploy the frontend on Vercel
1. Point the frontend at your Railway API: edit [`static/config.js`](static/config.js):
   ```js
   window.API_BASE = "https://<your-railway-domain>";
   ```
   Commit & push.
2. Vercel → **Add New → Project** → import this repo.
3. Settings are read from [`vercel.json`](vercel.json): no build step, serves the `static/`
   folder. (If Vercel asks, set **Framework Preset = Other**, **Output Directory = `static`**.)
4. Deploy. Your page is live at `https://<project>.vercel.app`, talking to the Railway API.

> Tip: you can test against any backend without redeploying via a URL override —
> `https://<project>.vercel.app/?api=https://<your-railway-domain>`.

### Single-host alternative (simplest)
The Rust binary already serves the UI **and** the API together, so you can skip Vercel
entirely and just deploy the Railway service — open its domain and the whole app is there.
In that mode `config.js` is served by the binary with `API_BASE` blank (same origin).

---

## API

### `POST /api/analyze`
`multipart/form-data`:

| field   | required | description                                   |
|---------|----------|-----------------------------------------------|
| `file`  | yes      | the statement file (any supported format)     |
| `query` | no       | name/keyword to search (blank = all txns)     |

```bash
curl -s \
  -F "file=@samples/sample_statement.csv" \
  -F "query=JOHN DOE" \
  http://localhost:8000/api/analyze | jq
```

### `GET /health`
Liveness probe → `{"status":"ok", ...}`.

### Response shape (abridged)
```jsonc
{
  "ok": true,
  "query": "JOHN DOE",
  "file": { "name": "...", "kind": "csv", "size_bytes": 655, "size_human": "655 B", "parts": [] },
  "currency": { "symbol": "₦", "code": "NGN", "name": "Nigerian Naira", "confidence": 1.0, "detected_by": "code/keyword" },
  "summary": {
    "total_transactions_scanned": 10,
    "matched_transactions": 6,
    "net_amount": 1340449.25,
    "net_formatted": "₦1,340,449.25",
    "overall_first_date": "2024-01-05",
    "overall_last_date": "2024-03-22",
    "overall_duration": { "days": 77, "weeks": 11, "months": 2, "years": 0, "human": "2 months 17 days" }
  },
  "debit":  { "label": "Debit",  "total": 18450.75,  "total_formatted": "₦18,450.75",  "count": 2, "average": 9225.38, "min": 3450.75, "max": 15000.0, "first_date": "...", "last_date": "...", "duration": { ... } },
  "credit": { "label": "Credit", "total": 1358900.0, "total_formatted": "₦1,358,900.00", "count": 4, ... },
  "matched": [ { "date": "2024-01-05", "description": "SALARY ...", "amount": 450000.0, "direction": "credit", "source": "..." } ],
  "matched_truncated": false,
  "warnings": [],
  "elapsed_ms": 0.23,
  "throughput_mb_s": 2.78
}
```

---

## How classification works (deterministic rules)

1. **Tabular sources (CSV/XLSX)** — the engine locates the header row, maps columns by name,
   and reads amounts from `Debit`/`Credit` columns, or a single signed `Amount` column
   (optionally guided by a `Dr/Cr` type column). With no header, it infers columns
   statistically (date column = most parseable dates, amount columns = most parseable
   numbers, description = longest text column).
2. **Text/PDF sources** — each line is scanned for a date and money tokens; when a line has
   two amounts the last is treated as the running balance. Direction comes from keywords
   (`salary`, `deposit`, `transfer from` → credit; `withdrawal`, `pos`, `transfer to`, `fee`
   → debit), then sign/parentheses as a fallback.
3. **Search** — case-insensitive substring match of the name/keyword against each
   transaction's description and raw line. Blank query analyses every transaction.

---

## Configuration

| Env var | Default | Meaning                          |
|---------|---------|----------------------------------|
| `PORT`  | `8000`  | HTTP listen port                 |
| `RUST_LOG` | `statement_analyzer=info` | log filter (e.g. `debug`) |
| `ADMIN_USER` | `admin` | admin dashboard login username |
| `ADMIN_PASSWORD` | *(random, logged at startup)* | admin dashboard login password — **set this in production** |
| `ADMIN_TOKEN` | *(random)* | internal session token issued after login (rarely set manually) |
| `DB_PATH` | `data/analytics.db` | SQLite analytics file (use a persistent volume in prod) |
| `IP_SALT` | *(random per start)* | salt for hashing IPs; set a fixed value to keep unique-IP counts stable across restarts |
| `MAX_UPLOAD_MB` | `25` | max upload size in MB (rejects larger to protect memory) |
| `RATE_ANALYZE_PER_MIN` | `20` | per-IP `/api/analyze` requests per minute |
| `RATE_FEEDBACK_PER_MIN` | `5` | per-IP `/api/feedback` requests per minute |
| `ALLOWED_ORIGINS` | *(unset = permissive)* | comma-separated origins for CORS lockdown, e.g. `https://yourstatementanalyzer.com,https://www.yourstatementanalyzer.com` |
| `SESSION_TTL_HOURS` | `12` | admin dashboard session lifetime |
| `EVENTS_RETENTION_DAYS` | `365` | analytics (visits + analyses) older than this are pruned (boot + every 6h) |
| `CACHE_TTL_MINUTES` | `20` | how long a parsed statement stays in memory for instant re-search before deletion |
| `RATE_TRACK_PER_MIN` | `40` | per-IP `/api/track` (page-visit) pings per minute |

---

## Security & admin

- **Rate limiting** — fixed-window per-IP limiter on `/api/analyze` and `/api/feedback` (429 when exceeded).
- **Security headers** — `Content-Security-Policy`, `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY`, `Referrer-Policy`, `Permissions-Policy` on every response.
- **Upload cap** — `MAX_UPLOAD_MB` (default 200 MB) rejects oversized bodies.
- **Privacy** — uploaded statements are processed **in memory only**, never written to disk, and
  dropped from the in-memory cache after `CACHE_TTL_MINUTES` (default 20). No transaction data is
  ever persisted. Raw IPs are never stored; only a salted SHA-256 hash (for unique counts) plus a
  coarse country/region/city. Visitors are identified by a random client-generated id (localStorage).
- **Analytics** — `/api/track` records a page **visit** (who / when / country-region-city / device /
  path / referrer) on load, distinct from an **analysis** event. The admin dashboard shows the full
  funnel: visits → unique people → analyses → conversion, with recent visits and analyses tables.
- **Admin dashboard** at **`/admin`** — **username + password login** (`ADMIN_USER` / `ADMIN_PASSWORD`).
  `POST /api/admin/login` validates the credentials (rate-limited, 10/min/IP) and returns a session
  token the dashboard uses for `GET /api/admin/stats`. Shows unique visitors, total analyses,
  country/region breakdown, daily activity, average rating, "would you pay" results, and recent reviews.
- **Feedback** — after a user's first analysis the UI asks for a star rating, a "would you pay for
  this?" answer, and an optional review; results are stored and surfaced in the admin dashboard.

> **Persistence on Railway:** the SQLite DB lives at `DB_PATH` (default `data/analytics.db`). Attach a
> Railway **Volume** mounted at `/app/data` (or set `DB_PATH` to the volume path) so analytics survive
> redeploys. Also set `ADMIN_USER`, `ADMIN_PASSWORD` and `IP_SALT` to fixed values in the service variables.

---

## Notes & limitations

- **Scanned/image PDFs** produce no text — OCR is intentionally not bundled (keeps it fast and
  deterministic). A warning is returned; wire in an OCR step before `engine::run` if needed.
- **RAR/7z/tar** rely on an external extractor being installed; otherwise a clear warning is
  returned and other files still process.
- Heuristic parsers are tuned for common Nigerian/UK/US/EU statement layouts. Unusual layouts
  may need a tweak to the keyword/column lists in `engine.rs` / `currency.rs`.

## License
MIT.
