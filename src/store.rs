//! Persistent analytics + feedback storage (SQLite via rusqlite).
//! Stores one row per successful analysis and one row per feedback submission,
//! and exposes aggregate stats for the admin dashboard.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    visitor   TEXT NOT NULL,
    ip_hash   TEXT NOT NULL,
    country   TEXT NOT NULL DEFAULT 'Unknown',
    region    TEXT NOT NULL DEFAULT '',
    city      TEXT NOT NULL DEFAULT '',
    file_kind TEXT NOT NULL DEFAULT '',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    scanned   INTEGER NOT NULL DEFAULT 0,
    matched   INTEGER NOT NULL DEFAULT 0,
    currency  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_events_visitor ON events(visitor);
CREATE INDEX IF NOT EXISTS idx_events_country ON events(country);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

CREATE TABLE IF NOT EXISTS feedback (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    visitor   TEXT NOT NULL DEFAULT '',
    country   TEXT NOT NULL DEFAULT 'Unknown',
    stars     INTEGER NOT NULL DEFAULT 0,
    review    TEXT NOT NULL DEFAULT '',
    would_pay TEXT NOT NULL DEFAULT '',
    price     TEXT NOT NULL DEFAULT ''
);
"#;

pub struct EventIn {
    pub visitor: String,
    pub ip_hash: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub file_kind: String,
    pub size_bytes: i64,
    pub scanned: i64,
    pub matched: i64,
    pub currency: String,
}

pub struct FeedbackIn {
    pub visitor: String,
    pub country: String,
    pub stars: i64,
    pub review: String,
    pub would_pay: String,
    pub price: String,
}

pub struct Store {
    // Separate connections: with WAL, reads (admin dashboard aggregations)
    // never block writes (event/feedback inserts). A slow stats query on the
    // read connection can't stall analytics recording on the write connection.
    write: Mutex<Connection>,
    read: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let write = Connection::open(path)?;
        write.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        write.execute_batch(SCHEMA)?;

        let read = Connection::open(path)?;
        read.execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")?;

        Ok(Self { write: Mutex::new(write), read: Mutex::new(read) })
    }

    /// Delete events older than `older_than_days`. Returns rows removed.
    pub fn prune(&self, older_than_days: i64) -> i64 {
        if let Ok(conn) = self.write.lock() {
            let cutoff = now_secs() - older_than_days.max(1) * 86_400;
            return conn
                .execute("DELETE FROM events WHERE ts < ?1", params![cutoff])
                .unwrap_or(0) as i64;
        }
        0
    }

    pub fn record_event(&self, e: &EventIn) {
        if let Ok(conn) = self.write.lock() {
            let _ = conn.execute(
                "INSERT INTO events (ts,visitor,ip_hash,country,region,city,file_kind,size_bytes,scanned,matched,currency)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    now_secs(), e.visitor, e.ip_hash, e.country, e.region, e.city,
                    e.file_kind, e.size_bytes, e.scanned, e.matched, e.currency
                ],
            );
        }
    }

    pub fn record_feedback(&self, f: &FeedbackIn) {
        if let Ok(conn) = self.write.lock() {
            let _ = conn.execute(
                "INSERT INTO feedback (ts,visitor,country,stars,review,would_pay,price)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![now_secs(), f.visitor, f.country, f.stars, f.review, f.would_pay, f.price],
            );
        }
    }

    pub fn stats(&self) -> Stats {
        let conn = match self.read.lock() {
            Ok(c) => c,
            Err(_) => return Stats::default(),
        };

        let scalar_i64 = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
        };

        let total_analyses = scalar_i64("SELECT COUNT(*) FROM events");
        let unique_visitors = scalar_i64("SELECT COUNT(DISTINCT visitor) FROM events");
        let unique_ips = scalar_i64("SELECT COUNT(DISTINCT ip_hash) FROM events");
        let total_bytes = scalar_i64("SELECT COALESCE(SUM(size_bytes),0) FROM events");
        let total_matched = scalar_i64("SELECT COALESCE(SUM(matched),0) FROM events");
        let total_scanned = scalar_i64("SELECT COALESCE(SUM(scanned),0) FROM events");

        let by_country = query_rows(&conn,
            "SELECT country, COUNT(*) c, COUNT(DISTINCT visitor) v FROM events GROUP BY country ORDER BY c DESC LIMIT 50",
            |r| Ok(CountryStat {
                country: r.get(0)?,
                count: r.get(1)?,
                visitors: r.get(2)?,
            }));

        let by_day = query_rows(&conn,
            "SELECT date(ts,'unixepoch') d, COUNT(*) c, COUNT(DISTINCT visitor) v FROM events GROUP BY d ORDER BY d DESC LIMIT 30",
            |r| Ok(DayStat { day: r.get(0)?, count: r.get(1)?, visitors: r.get(2)? }));

        let recent_events = query_rows(&conn,
            "SELECT ts,visitor,country,region,file_kind,size_bytes,scanned,matched,currency FROM events ORDER BY id DESC LIMIT 60",
            |r| Ok(RecentEvent {
                ts: r.get(0)?,
                visitor: short_visitor(&r.get::<_, String>(1)?),
                country: r.get(2)?,
                region: r.get(3)?,
                file_kind: r.get(4)?,
                size_bytes: r.get(5)?,
                scanned: r.get(6)?,
                matched: r.get(7)?,
                currency: r.get(8)?,
            }));

        // Feedback aggregates
        let feedback_count = scalar_i64("SELECT COUNT(*) FROM feedback WHERE stars > 0");
        let avg_stars = conn
            .query_row("SELECT AVG(stars) FROM feedback WHERE stars > 0", [], |r| r.get::<_, f64>(0))
            .unwrap_or(0.0);
        let pay_yes = scalar_i64("SELECT COUNT(*) FROM feedback WHERE would_pay='yes'");
        let pay_maybe = scalar_i64("SELECT COUNT(*) FROM feedback WHERE would_pay='maybe'");
        let pay_no = scalar_i64("SELECT COUNT(*) FROM feedback WHERE would_pay='no'");

        let reviews = query_rows(&conn,
            "SELECT ts,country,stars,review,would_pay,price FROM feedback ORDER BY id DESC LIMIT 100",
            |r| Ok(Review {
                ts: r.get(0)?,
                country: r.get(1)?,
                stars: r.get(2)?,
                review: r.get(3)?,
                would_pay: r.get(4)?,
                price: r.get(5)?,
            }));

        Stats {
            total_analyses,
            unique_visitors,
            unique_ips,
            total_bytes,
            total_scanned,
            total_matched,
            by_country,
            by_day,
            recent_events,
            feedback_count,
            avg_stars: (avg_stars * 100.0).round() / 100.0,
            pay_yes,
            pay_maybe,
            pay_no,
            reviews,
        }
    }
}

fn query_rows<T, F>(conn: &Connection, sql: &str, mapper: F) -> Vec<T>
where
    F: Fn(&rusqlite::Row) -> rusqlite::Result<T>,
{
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(sql) {
        if let Ok(rows) = stmt.query_map([], |r| mapper(r)) {
            for row in rows.flatten() {
                out.push(row);
            }
        }
    }
    out
}

fn short_visitor(v: &str) -> String {
    v.chars().take(8).collect()
}

pub fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ----------------------- Serializable stats -----------------------

#[derive(Default, Serialize)]
pub struct Stats {
    pub total_analyses: i64,
    pub unique_visitors: i64,
    pub unique_ips: i64,
    pub total_bytes: i64,
    pub total_scanned: i64,
    pub total_matched: i64,
    pub by_country: Vec<CountryStat>,
    pub by_day: Vec<DayStat>,
    pub recent_events: Vec<RecentEvent>,
    pub feedback_count: i64,
    pub avg_stars: f64,
    pub pay_yes: i64,
    pub pay_maybe: i64,
    pub pay_no: i64,
    pub reviews: Vec<Review>,
}

#[derive(Serialize)]
pub struct CountryStat {
    pub country: String,
    pub count: i64,
    pub visitors: i64,
}

#[derive(Serialize)]
pub struct DayStat {
    pub day: String,
    pub count: i64,
    pub visitors: i64,
}

#[derive(Serialize)]
pub struct RecentEvent {
    pub ts: i64,
    pub visitor: String,
    pub country: String,
    pub region: String,
    pub file_kind: String,
    pub size_bytes: i64,
    pub scanned: i64,
    pub matched: i64,
    pub currency: String,
}

#[derive(Serialize)]
pub struct Review {
    pub ts: i64,
    pub country: String,
    pub stars: i64,
    pub review: String,
    pub would_pay: String,
    pub price: String,
}
