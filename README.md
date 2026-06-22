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

Upload limit is **1 GiB** (`MAX_UPLOAD` in `src/main.rs`).

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
