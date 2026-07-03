//! Internal transaction model and the JSON output schema.

use chrono::NaiveDate;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Debit,
    Credit,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub date: Option<NaiveDate>,
    pub description: String,
    pub amount: f64, // magnitude, always positive
    pub direction: Direction,
    pub balance: Option<f64>,
    pub raw: String,
    pub source: String, // which file / sheet it came from
    pub line_no: usize,
}

// ----------------------- Output schema -----------------------

#[derive(Debug, Serialize)]
pub struct DurationBreakdown {
    pub days: i64,
    pub weeks: i64,
    pub months: i64,
    pub years: i64,
    pub human: String,
}

#[derive(Debug, Serialize)]
pub struct SideStats {
    pub label: String,
    pub total: f64,
    pub total_formatted: String,
    pub count: usize,
    pub average: f64,
    pub min: f64,
    pub max: f64,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    pub duration: Option<DurationBreakdown>,
}

#[derive(Debug, Serialize)]
pub struct MatchedTxn {
    pub date: Option<String>,
    pub description: String,
    pub amount: f64,
    pub direction: Direction,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct FileMeta {
    pub name: String,
    pub kind: String,
    pub size_bytes: usize,
    pub size_human: String,
    pub parts: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub total_transactions_scanned: usize,
    pub matched_transactions: usize,
    pub net_amount: f64,
    pub net_formatted: String,
    pub overall_first_date: Option<String>,
    pub overall_last_date: Option<String>,
    pub overall_duration: Option<DurationBreakdown>,
}

#[derive(Debug, Serialize)]
pub struct Interpreted {
    pub keyword: String,
    pub direction: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub human: String,
    pub smart: bool,
}

#[derive(Debug, Serialize)]
pub struct AnalysisResult {
    pub ok: bool,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_id: Option<String>,
    pub interpreted: Interpreted,
    pub file: FileMeta,
    pub currency: crate::currency::Currency,
    pub summary: Summary,
    pub debit: SideStats,
    pub credit: SideStats,
    pub matched: Vec<MatchedTxn>,
    pub matched_truncated: bool,
    pub warnings: Vec<String>,
    pub elapsed_ms: f64,
    pub throughput_mb_s: f64,
}
