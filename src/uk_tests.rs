//! UK bank statement fixtures (all fictional, see tests/fixtures/uk/generate.py).
//! Every layout carries the same ten transactions: 7 out totalling 1,308.59
//! and 3 in totalling 2,369.99.
use crate::engine::run;
use crate::model::{AnalysisResult, Direction};

fn fx(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/uk/{name}", env!("CARGO_MANIFEST_DIR"))).expect(name)
}

fn assert_statement(name: &str, r: &AnalysisResult) {
    assert_eq!((r.debit.count, r.credit.count), (7, 3), "{name}: counts, warnings {:?}", r.warnings);
    assert!((r.debit.total - 1308.59).abs() < 0.01, "{name}: money out was {}", r.debit.total);
    assert!((r.credit.total - 2369.99).abs() < 0.01, "{name}: money in was {}", r.credit.total);
    assert_eq!(r.summary.overall_first_date.as_deref(), Some("2026-09-01"), "{name}: first date");
    assert_eq!(r.summary.overall_last_date.as_deref(), Some("2026-09-28"), "{name}: last date");
}

const CSV_LIKE: &[&str] = &[
    "barclays.csv", "hsbc.csv", "lloyds.csv", "natwest.csv", "nationwide.csv", "monzo.csv", "starling.csv",
    "revolut.csv", "semicolon_unsigned.csv", "santander.txt", "statement.qif", "statement.ofx",
];
const PDFS: &[&str] = &["pdf_grouped.pdf", "pdf_money_out_in.pdf", "pdf_signed.pdf", "pdf_symbols.pdf"];

fn have_pdftotext() -> bool {
    std::process::Command::new("pdftotext").arg("-v").output().is_ok()
}

#[test]
fn uk_bank_exports_parse_correctly() {
    for name in CSV_LIKE {
        assert_statement(name, &run(name, &fx(name), ""));
    }
}

#[test]
fn uk_exports_show_real_descriptions_and_are_searchable() {
    for name in CSV_LIKE {
        let r = run(name, &fx(name), "tesco");
        assert_eq!(r.summary.matched_transactions, 1, "{name}: tesco search");
        let d = r.matched[0].description.to_lowercase();
        assert!(d.contains("tesco"), "{name}: description shown was {d:?}");
        assert_eq!(r.matched[0].direction, Direction::Debit, "{name}");
        assert!((r.matched[0].amount - 23.50).abs() < 0.001, "{name}");
    }
}

#[test]
fn uk_currency_is_sterling_or_plain() {
    for name in ["lloyds.csv", "barclays.csv", "nationwide.csv", "monzo.csv", "starling.csv", "revolut.csv", "santander.txt", "statement.ofx"] {
        assert_eq!(run(name, &fx(name), "").currency.code, "GBP", "{name}");
    }
    // Nothing in the file names a currency: plain numbers, never a "?" symbol.
    let r = run("hsbc.csv", &fx("hsbc.csv"), "");
    assert_eq!(r.currency.symbol, "");
    assert!(!r.debit.total_formatted.contains('?'));
}

#[test]
fn uk_pdf_statements_parse_correctly() {
    if !have_pdftotext() {
        eprintln!("pdftotext not installed, skipping layout PDF checks");
        return;
    }
    for name in PDFS {
        let r = run(name, &fx(name), "");
        assert_statement(name, &r);
        assert_eq!(r.currency.code, "GBP", "{name}");
        // Merchant names starting with CR / DR must not flip the direction.
        for (kw, dir) in [("croydon", Direction::Debit), ("martens", Direction::Debit), ("salary", Direction::Credit), ("refund", Direction::Credit)] {
            let q = run(name, &fx(name), kw);
            assert_eq!(q.summary.matched_transactions, 1, "{name}: {kw}");
            assert_eq!(q.matched[0].direction, dir, "{name}: {kw}");
        }
        // Wrapped description lines stay with their transaction.
        let t = run(name, &fx(name), "tesco");
        assert_eq!(t.summary.matched_transactions, 1, "{name}: tesco");
        assert!((t.matched[0].amount - 23.50).abs() < 0.001, "{name}: tesco amount");
        // Account numbers, limits and balance summaries are not transactions.
        assert!(r.matched.iter().all(|m| m.amount < 5000.0), "{name}: stray amount");
    }
}

#[test]
fn pdf_without_layout_tool_still_gets_totals_from_the_balance_trail() {
    for name in ["pdf_money_out_in.pdf", "pdf_signed.pdf", "pdf_symbols.pdf"] {
        let lines = crate::extract::pdf_plain_lines(&fx(name)).expect(name);
        let mut out = Vec::new();
        crate::engine::text_to_txns(name, &lines, &mut out);
        let debit: f64 = out.iter().filter(|t| t.direction == Direction::Debit).map(|t| t.amount).sum();
        let credit: f64 = out.iter().filter(|t| t.direction == Direction::Credit).map(|t| t.amount).sum();
        assert_eq!(out.len(), 10, "{name}");
        assert!((debit - 1308.59).abs() < 0.01 && (credit - 2369.99).abs() < 0.01, "{name}: {debit} / {credit}");
    }
}

#[test]
fn newest_first_statement_uses_the_balance_trail() {
    // Unsigned amounts, newest row first: only the balance shows the direction.
    let csv = "Date,Details,Amount,Balance\n\
               05/09/2026,COFFEE SHOP,4.50,1245.50\n\
               04/09/2026,J SMITH,250.00,1250.00\n\
               03/09/2026,GYM MEMBERSHIP,40.00,1000.00\n\
               02/09/2026,BOOKSHOP,10.00,1040.00\n";
    // The oldest row has no earlier balance to compare with; the other three are settled.
    for (kw, dir) in [("coffee", Direction::Debit), ("smith", Direction::Credit), ("gym", Direction::Debit)] {
        assert_eq!(run("t.csv", csv.as_bytes(), kw).matched[0].direction, dir, "{kw}");
    }
}

#[test]
fn amount_signs_in_uk_styles() {
    use crate::util::parse_amount;
    for neg in ["-12.50", "£-12.50", "-£12.50", "12.50-", "(12.50)", "12.50 DR", "12.50 OD", "\u{2212}12.50"] {
        let m = parse_amount(neg).unwrap();
        assert!(m.value < 0.0 && (m.magnitude() - 12.5).abs() < 0.001, "{neg}");
    }
    for pos in ["12.50", "£12.50", "1,234.50", "12.50 CR"] {
        assert!(parse_amount(pos).unwrap().value > 0.0, "{pos}");
    }
}

#[test]
fn unreadable_pdf_reports_a_clear_message_instead_of_failing() {
    let r = run("broken.pdf", b"%PDF-1.7 this is not really a pdf", "");
    assert_eq!(r.summary.total_transactions_scanned, 0);
    assert!(r.warnings.iter().any(|w| w.contains("could not be read") || w.contains("no readable text")), "{:?}", r.warnings);
}

#[test]
fn grouped_by_day_statement_with_wrapped_details() {
    // Date printed once per day, the amount on the last line of each entry,
    // and the balance only at the end of the day.
    let txt = "\
Date        Payment type and details                 Paid out        Paid in        Balance
14 Sep 26   BALANCE BROUGHT FORWARD                                                 1,000.00
15 Sep 26   VIS   TESCO STORES 3297
                  LONDON                                23.50
            DD    BRITISH GAS                           45.00
            CR    ACME LTD
                  SALARY SEP                                          2,100.00      3,031.50
16 Sep 26   )))   TFL TRAVEL CH                          8.10
            ATM   CASH HIGH ST                          60.00                       2,963.40
16 Sep 26   BALANCE CARRIED FORWARD                                                 2,963.40
";
    let r = run("statement.txt", txt.as_bytes(), "");
    assert_eq!((r.debit.count, r.credit.count), (4, 1), "{:?}", r.matched);
    assert!((r.debit.total - 136.60).abs() < 0.01 && (r.credit.total - 2100.0).abs() < 0.01);
    let t = run("statement.txt", txt.as_bytes(), "tesco");
    assert_eq!(t.summary.matched_transactions, 1);
    assert_eq!(t.matched[0].date.as_deref(), Some("2026-09-15"));
    assert!((t.matched[0].amount - 23.50).abs() < 0.001);
    let s = run("statement.txt", txt.as_bytes(), "acme");
    assert_eq!((s.matched[0].direction, s.matched[0].date.as_deref()), (Direction::Credit, Some("2026-09-15")));
    assert_eq!(run("statement.txt", txt.as_bytes(), "cash high").matched[0].date.as_deref(), Some("2026-09-16"));
}

#[test]
fn yearless_dates_and_trailing_detail_lines() {
    let txt = "\
Statement period 1 September 2026 to 30 September 2026
Date      Description                         Money out    Money in     Balance
1 Sep     Start balance                                                 1,500.00
2 Sep     Card Payment to Tesco Stores           23.50                  1,476.50
          On 01 Sep
3 Sep     Received From J Smith                              250.00     1,726.50
          Ref: RENT SHARE
";
    let r = run("statement.txt", txt.as_bytes(), "");
    assert_eq!((r.debit.count, r.credit.count), (1, 1), "{:?}", r.matched);
    assert_eq!(run("statement.txt", txt.as_bytes(), "tesco").matched[0].date.as_deref(), Some("2026-09-02"));
    let c = run("statement.txt", txt.as_bytes(), "rent share");
    assert_eq!(c.summary.matched_transactions, 1);
    assert_eq!((c.matched[0].direction, c.matched[0].date.as_deref()), (Direction::Credit, Some("2026-09-03")));
}
