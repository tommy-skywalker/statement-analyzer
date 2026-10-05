#!/usr/bin/env python3
"""Generate fictional UK bank statement fixtures in the export layouts of the
major UK banks. Every name, number and amount is invented.

Run from this directory:  python3 generate.py
PDFs need Google Chrome (headless print-to-PDF).
"""
import os, subprocess, html

HERE = os.path.dirname(os.path.abspath(__file__))
OPEN = 1500.00
# (day, description, type code, amount; negative = money out)
TX = [
    (1, "CARD PAYMENT TO TESCO STORES 3297", "VIS", -23.50),
    (2, "DIRECT DEBIT BRITISH GAS", "DD", -45.00),
    (3, "FASTER PAYMENT FROM J SMITH", "FPI", 250.00),
    (5, "CROYDON COUNCIL TAX", "DD", -132.00),
    (8, "DR MARTENS LONDON", "VIS", -89.99),
    (15, "ACME LTD SALARY", "BGC", 2100.00),
    (18, "STANDING ORDER LANDLORD RENT", "SO", -950.00),
    (22, "TFL TRAVEL CHARGE", "VIS", -8.10),
    (25, "AMAZON REFUND", "VIS", 19.99),
    (28, "CASH WITHDRAWAL ATM HIGH ST", "ATM", -60.00),
]
MON = "September"

def rows():
    bal = OPEN
    for d, desc, code, amt in TX:
        bal = round(bal + amt, 2)
        yield d, desc, code, amt, bal

def write(name, text, encoding="utf-8"):
    with open(os.path.join(HERE, name), "w", encoding=encoding, newline="") as f:
        f.write(text)

def money(v):
    return f"{v:,.2f}"

# ---------- CSV exports ----------
def barclays():
    out = ["Number,Date,Account,Amount,Subcategory,Memo"]
    for d, desc, code, amt, bal in rows():
        out.append(f" ,{d:02}/09/2026,20-00-00 12345678,{amt:.2f},{'Card Purchase' if amt < 0 else 'Counter Credit'},{desc}")
    write("barclays.csv", "\n".join(out) + "\n")

def hsbc():  # no header row
    out = [f'{d:02}/09/2026,{desc},"{amt:,.2f}"' for d, desc, code, amt, bal in rows()]
    write("hsbc.csv", "\n".join(out) + "\n")

def lloyds():
    out = ["Transaction Date,Transaction Type,Sort Code,Account Number,Transaction Description,Debit Amount,Credit Amount,Balance"]
    for d, desc, code, amt, bal in rows():
        out.append(f"{d:02}/09/2026,{code},'30-00-00,12345678,{desc},{-amt if amt < 0 else ''},{amt if amt > 0 else ''},{bal:.2f}")
    write("lloyds.csv", "\n".join(out) + "\n")

def natwest():
    out = ["Date, Type, Description, Value, Balance, Account Name, Account Number", ""]
    for d, desc, code, amt, bal in rows():
        out.append(f"{d:02}/09/2026,{'D/D' if code == 'DD' else 'POS'},\"'{desc}\",{amt:.2f},{bal:.2f},\"'SMITH J\",\"'600000-12345678\",")
    write("natwest.csv", "\n".join(out) + "\n")

def nationwide():  # Windows-1252 with pound signs and a preamble
    out = ['"Account Name:","FlexAccount ****45678"', '"Account Balance:","£2561.40"', '"Available Balance: ","£2561.40"', "",
           '"Date","Transaction type","Description","Paid out","Paid in","Balance"']
    for d, desc, code, amt, bal in rows():
        out.append(f'"{d:02} Sep 2026","{ "Visa purchase" if amt < 0 else "Bank credit"}","{desc}","{("£%.2f" % -amt) if amt < 0 else ""}","{("£%.2f" % amt) if amt > 0 else ""}","£{bal:.2f}"')
    write("nationwide.csv", "\r\n".join(out) + "\r\n", encoding="cp1252")

def monzo():
    out = ["Transaction ID,Date,Time,Type,Name,Emoji,Category,Amount,Currency,Local amount,Local currency,Notes and #tags,Address,Receipt,Description,Category split,Money Out,Money In"]
    for i, (d, desc, code, amt, bal) in enumerate(rows()):
        typ = "Direct Debit" if code == "DD" else ("Faster payment" if amt > 0 else "Card payment")
        out.append(f"tx_0000A{i:04}xYz,{d:02}/09/2026,{9 + i}:15:0{i % 10},{typ},{desc.title()},,General,{amt:.2f},GBP,{amt:.2f},GBP,,,,{desc},,{amt if amt < 0 else ''},{amt if amt > 0 else ''}")
    write("monzo.csv", "\n".join(out) + "\n")

def starling():
    out = ["Date,Counter Party,Reference,Type,Amount (GBP),Balance (GBP),Spending Category,Notes"]
    for d, desc, code, amt, bal in rows():
        out.append(f"{d:02}/09/2026,{desc.title()},REF{d:04},{'DIRECT DEBIT' if code == 'DD' else 'FASTER PAYMENT'},{amt:.2f},{bal:.2f},GENERAL,")
    write("starling.csv", "\n".join(out) + "\n")

def revolut():
    out = ["Type,Product,Started Date,Completed Date,Description,Amount,Fee,Currency,State,Balance"]
    for d, desc, code, amt, bal in rows():
        out.append(f"{'TOPUP' if amt > 0 else 'CARD_PAYMENT'},Current,2026-09-{d:02} 10:11:12,2026-09-{d:02} 18:01:02,{desc.title()},{amt:.2f},0.00,GBP,COMPLETED,{bal:.2f}")
    write("revolut.csv", "\n".join(out) + "\n")

def unsigned_with_balance():  # one positive Amount column; only the balance shows direction
    out = ["Date;Details;Amount;Balance"]
    for d, desc, code, amt, bal in rows():
        out.append(f"{d:02}.09.2026;{desc};{abs(amt):.2f};{bal:.2f}")
    write("semicolon_unsigned.csv", "\n".join(out) + "\n")

def santander_txt():  # key/value blocks, Windows-1252 with non-breaking spaces
    out = ["From:\xa001/09/2026\xa0to\xa030/09/2026", "", "Account:\xa0XXXX XXXX XXXX 5678", ""]
    for d, desc, code, amt, bal in rows():
        out += [f"Date:\xa0{d:02}/09/2026", f"Description:\xa0{desc}", f"Amount:\xa0{amt:.2f}\xa0GBP", f"Balance:\xa0{bal:.2f}\xa0GBP", ""]
    write("santander.txt", "\r\n".join(out), encoding="cp1252")

def qif():
    out = ["!Type:Bank"]
    for d, desc, code, amt, bal in rows():
        out += [f"D{d:02}/09/2026", f"T{amt:.2f}", f"P{desc}", "^"]
    write("statement.qif", "\n".join(out) + "\n")

def ofx():
    body = "".join(
        f"<STMTTRN><TRNTYPE>{'CREDIT' if amt > 0 else 'DEBIT'}<DTPOSTED>202609{d:02}<TRNAMT>{amt:.2f}<FITID>2026{d:04}<NAME>{html.escape(desc)}</STMTTRN>\n"
        for d, desc, code, amt, bal in rows())
    write("statement.ofx", "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>GBP\n<BANKTRANLIST>\n" + body + "</BANKTRANLIST></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>\n")

# ---------- PDF statements (HTML printed by headless Chrome) ----------
CSS = """<style>
body{font-family:Helvetica,Arial,sans-serif;font-size:10pt;margin:28pt}
h1{font-size:15pt} table{border-collapse:collapse;width:100%} th{text-align:left;border-bottom:1px solid #000;padding:4pt 6pt}
td{padding:4pt 6pt;vertical-align:top} .n{text-align:right} .small{font-size:8pt;color:#444}
</style>"""

def pdf(name, body):
    h = os.path.join(HERE, name + ".html")
    with open(h, "w", encoding="utf-8") as f:
        f.write("<!doctype html><meta charset='utf-8'>" + CSS + body)
    chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    subprocess.run([chrome, "--headless", "--disable-gpu", "--no-pdf-header-footer",
                    f"--print-to-pdf={os.path.join(HERE, name + '.pdf')}", "file://" + h],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    os.remove(h)

HEAD = ("<h1>{bank}</h1><p>Mr J Smith<br>Flat 2, 10 Example Road<br>London E1 1AA</p>"
        "<p class='small'>Sort code 40-00-00 &nbsp; Account number 12345678 &nbsp; IBAN GB00EXMP40000012345678</p>"
        "<p>Statement period 1 September 2026 to 30 September 2026</p>"
        "<p>Arranged overdraft limit £500.00 &nbsp; Interest rate 0.00% AER</p>")
FOOT = "<p class='small'>Your eligible deposits are protected up to £85,000 by the Financial Services Compensation Scheme. Page 1 of 1</p>"

def pdf_grouped():
    # HSBC-like: short dates, type codes, wrapped details, balance only on some rows
    b = HEAD.format(bank="Example Bank UK") + "<table><tr><th>Date</th><th>Payment type and details</th><th class='n'>Paid out</th><th class='n'>Paid in</th><th class='n'>Balance</th></tr>"
    b += f"<tr><td>31 Aug 26</td><td>BALANCE BROUGHT FORWARD</td><td></td><td></td><td class='n'>{money(OPEN)}</td></tr>"
    for i, (d, desc, code, amt, bal) in enumerate(rows()):
        first, _, rest = desc.partition(" TO ") if " TO " in desc else (desc, "", "")
        details = f"{code} &nbsp; {desc}" if not rest else f"{code} &nbsp; {first} TO<br>{rest}"
        show_bal = i % 3 == 2 or i == len(TX) - 1
        b += (f"<tr><td>{d:02} Sep 26</td><td>{details}</td><td class='n'>{money(-amt) if amt < 0 else ''}</td>"
              f"<td class='n'>{money(amt) if amt > 0 else ''}</td><td class='n'>{money(bal) if show_bal else ''}</td></tr>")
    b += f"<tr><td>30 Sep 26</td><td>BALANCE CARRIED FORWARD</td><td></td><td></td><td class='n'>{money(bal)}</td></tr></table>" + FOOT
    pdf("pdf_grouped", b)

def pdf_money_out_in():
    # Barclays/Lloyds-like: yearless dates, Money out / Money in / Balance
    b = HEAD.format(bank="Sample High Street Bank") + f"<p>Start balance £{money(OPEN)} &nbsp; Money in £2,369.99 &nbsp; Money out £1,308.59 &nbsp; End balance £2,561.40</p>"
    b += "<table><tr><th>Date</th><th>Description</th><th class='n'>Money out</th><th class='n'>Money in</th><th class='n'>Balance</th></tr>"
    b += f"<tr><td>1 Sep</td><td>Start balance</td><td></td><td></td><td class='n'>{money(OPEN)}</td></tr>"
    for d, desc, code, amt, bal in rows():
        b += (f"<tr><td>{d} Sep</td><td>{desc.title()}</td><td class='n'>{money(-amt) if amt < 0 else ''}</td>"
              f"<td class='n'>{money(amt) if amt > 0 else ''}</td><td class='n'>{money(bal)}</td></tr>")
    b += "</table>" + FOOT
    pdf("pdf_money_out_in", b)

def pdf_signed():
    # Monzo-like: numeric dates, one signed amount column
    b = HEAD.format(bank="Example Digital Bank") + "<table><tr><th>Date</th><th>Description</th><th class='n'>(GBP) Amount</th><th class='n'>(GBP) Balance</th></tr>"
    for d, desc, code, amt, bal in rows():
        b += f"<tr><td>{d:02}/09/2026</td><td>{desc.title()}</td><td class='n'>{amt:,.2f}</td><td class='n'>{money(bal)}</td></tr>"
    b += "</table>" + FOOT
    pdf("pdf_signed", b)

def pdf_symbols():
    # Revolut-like: "Sep 1, 2026" dates and pound signs on every amount
    b = HEAD.format(bank="Example Money App") + "<table><tr><th>Date</th><th>Description</th><th class='n'>Money out</th><th class='n'>Money in</th><th class='n'>Balance</th></tr>"
    for d, desc, code, amt, bal in rows():
        b += (f"<tr><td>Sep {d}, 2026</td><td>{desc.title()}</td><td class='n'>{'£' + money(-amt) if amt < 0 else ''}</td>"
              f"<td class='n'>{'£' + money(amt) if amt > 0 else ''}</td><td class='n'>£{money(bal)}</td></tr>")
    b += "</table>" + FOOT
    pdf("pdf_symbols", b)

if __name__ == "__main__":
    for f in (barclays, hsbc, lloyds, natwest, nationwide, monzo, starling, revolut, unsigned_with_balance,
              santander_txt, qif, ofx, pdf_grouped, pdf_money_out_in, pdf_signed, pdf_symbols):
        f()
    print("fixtures written to", HERE)
