//! `verify --report`: one self-contained HTML page for whoever signs off the migration.

use std::{fmt::Write as _, path::Path, time::SystemTime};

use anyhow::{Context, Result};

use crate::verify::Check;

pub struct Meta<'a> {
    pub mapping: &'a Path,
    pub dump: &'a Path,
    pub db: &'a str,
}

pub fn write(path: &Path, checks: &[Check], meta: &Meta) -> Result<()> {
    std::fs::write(path, render(checks, meta, &now_utc())).with_context(|| format!("write {}", path.display()))
}

fn render(checks: &[Check], meta: &Meta, generated: &str) -> String {
    let pass = checks.iter().all(Check::ok);
    let sum = |f: fn(&Check) -> i64| checks.iter().map(f).sum::<i64>();
    let failing = checks.iter().filter(|c| !c.ok()).count();

    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>lift verify report</title>
<style>{CSS}</style></head><body><main>
<p class="eyebrow">lift verify · {generated}</p>
<h1><span class="verdict {cls}">{verdict}</span> {headline}</h1>
<dl class="meta">
  <dt>Mapping</dt><dd><code>{mapping}</code></dd>
  <dt>Dump</dt><dd><code>{dump}</code></dd>
  <dt>Database</dt><dd><code>{db}</code></dd>
  <dt>lift</dt><dd><code>{version}</code></dd>
</dl>
<div class="stats">
  {s1}{s2}{s3}{s4}{s5}{s6}
</div>
<p class="note">Every document was converted again and compared with the stored row, value by value. <em>Rejected</em> documents can't be stored (reasons below and in <code>lift-rejects/</code>); <em>missing</em> and <em>changed</em> rows should be zero; <em>extra</em> rows didn't come from the dump (usually seed data). Record the fingerprints: re-running verify later and getting the same ones shows nothing moved.</p>
"#,
        cls = if pass { "pass" } else { "fail" },
        verdict = if pass { "PASS" } else { "FAIL" },
        headline = if pass {
            format!("all {} tables match the dump", checks.len())
        } else {
            format!("{failing} of {} tables don't match the dump", checks.len())
        },
        mapping = esc(&meta.mapping.display().to_string()),
        dump = esc(&meta.dump.display().to_string()),
        db = esc(&redact(meta.db)),
        version = env!("CARGO_PKG_VERSION"),
        s1 = stat("tables", checks.len() as i64, false),
        s2 = stat("rows expected", sum(|c| c.expected), false),
        s3 = stat("rejected", sum(|c| c.rejected as i64), false),
        s4 = stat("missing", sum(|c| c.missing), true),
        s5 = stat("changed", sum(|c| c.changed), true),
        s6 = stat("extra", sum(|c| c.extra), false),
    );

    h.push_str(
        r#"<div class="scroll"><table><thead><tr><th></th><th>Table</th><th>Docs</th><th>Rejected</th><th>Expected</th><th>Missing</th><th>Changed</th><th>Extra</th><th>Fingerprint</th></tr></thead><tbody>"#,
    );
    for c in checks {
        let status = if !c.ok() {
            r#"<span class="dot bad" title="doesn't match">✕</span>"#
        } else {
            r#"<span class="dot ok" title="matches">✓</span>"#
        };
        let name = if c.child { format!(r#"<span class="child">↳</span> {}"#, esc(&c.table)) } else { esc(&c.table) };
        let fp = if c.not_in_dump {
            r#"<span class="muted">not in dump</span>"#.to_string()
        } else {
            format!("<code>{}</code>", esc(&c.fingerprint.chars().take(12).collect::<String>()))
        };
        let _ = write!(
            h,
            "<tr{cls}><td>{status}</td><td>{name}</td><td>{}</td><td>{}</td><td>{}</td><td{}>{}</td><td{}>{}</td><td>{}</td><td>{fp}</td></tr>",
            c.docs,
            c.rejected,
            c.expected,
            bad_if(c.missing),
            c.missing,
            bad_if(c.changed),
            c.changed,
            c.extra,
            cls = if c.ok() { "" } else { r#" class="row-bad""# },
        );
    }
    h.push_str("</tbody></table></div>");

    let detailed: Vec<&Check> = checks
        .iter()
        .filter(|c| !c.ok() || !c.reasons.is_empty() || !c.zero_filled.is_empty() || !c.zero_ids.is_empty())
        .collect();
    if !detailed.is_empty() {
        h.push_str("<h2>Details</h2>");
    }
    for c in detailed {
        let _ = write!(h, "<details{}><summary>{}</summary>", if c.ok() { "" } else { " open" }, esc(&c.table));
        if !c.changed_columns.is_empty() {
            h.push_str("<h3>Columns that differ</h3><ul>");
            for (col, n) in &c.changed_columns {
                let _ = write!(h, "<li><code>{}</code> on {n} row(s)</li>", esc(col));
            }
            h.push_str("</ul>");
        }
        if !c.samples.is_empty() {
            h.push_str("<h3>Examples</h3><ul>");
            for s in &c.samples {
                let _ = write!(h, "<li><code>{}</code></li>", esc(s));
            }
            h.push_str("</ul>");
        }
        list(&mut h, "Why documents were rejected", &c.reasons, "");
        list(&mut h, "Filled with Go's zero value", &c.zero_filled, " (field missing)");
        list(&mut h, "Zero ObjectId stored as NULL", &c.zero_ids, "");
        h.push_str("</details>");
    }
    h.push_str("</main></body></html>\n");
    h
}

fn list(h: &mut String, title: &str, items: &[(String, usize)], suffix: &str) {
    if items.is_empty() {
        return;
    }
    let _ = write!(h, "<h3>{title}</h3><ul>");
    for (what, n) in items.iter().take(20) {
        let _ = write!(h, r#"<li><span class="count">{n}</span> {}{suffix}</li>"#, esc(what));
    }
    if items.len() > 20 {
        let _ = write!(h, r#"<li class="muted">… {} more</li>"#, items.len() - 20);
    }
    h.push_str("</ul>");
}

fn stat(label: &str, n: i64, must_be_zero: bool) -> String {
    let cls = if must_be_zero && n > 0 { " bad" } else { "" };
    format!(r#"<div class="stat{cls}"><span class="n">{n}</span><span class="l">{label}</span></div>"#)
}

fn bad_if(n: i64) -> &'static str {
    if n > 0 { r#" class="bad""# } else { "" }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Drop the password from a key=value or URL connection string.
fn redact(db: &str) -> String {
    if let Some((scheme, rest)) = db.split_once("://") {
        if let Some((creds, host)) = rest.split_once('@') {
            let user = creds.split(':').next().unwrap_or("");
            return format!("{scheme}://{user}@{host}");
        }
        return db.to_string();
    }
    db.split_whitespace().filter(|kv| !kv.to_ascii_lowercase().starts_with("password=")).collect::<Vec<_>>().join(" ")
}

/// "2026-11-08 14:05 UTC", without pulling in a date crate.
fn now_utc() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

const CSS: &str = r#"
:root { --bg:#fbfbfa; --fg:#1d1d1b; --muted:#6b6b66; --line:#e4e3df; --card:#fff; --ok:#1f7a4d; --okbg:#e6f4ec; --bad:#b42318; --badbg:#fdecea; --code:#f2f1ee; }
@media (prefers-color-scheme: dark) { :root { --bg:#151514; --fg:#ecebe7; --muted:#9a9993; --line:#2c2c2a; --card:#1c1c1b; --ok:#5cc28f; --okbg:#173326; --bad:#ff8a7a; --badbg:#3a1714; --code:#262624; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif; }
main { max-width:1100px; margin:0 auto; padding:40px 16px 64px; }
.eyebrow { color:var(--muted); font-size:13px; margin:0 0 6px; letter-spacing:.02em; }
h1 { font-size:26px; line-height:1.25; margin:0 0 18px; font-weight:650; }
h2 { font-size:18px; margin:36px 0 12px; }
h3 { font-size:13px; text-transform:uppercase; letter-spacing:.05em; color:var(--muted); margin:16px 0 6px; }
.verdict { display:inline-block; padding:2px 10px; border-radius:6px; font-size:15px; vertical-align:4px; margin-right:6px; letter-spacing:.04em; }
.verdict.pass { background:var(--okbg); color:var(--ok); } .verdict.fail { background:var(--badbg); color:var(--bad); }
.meta { display:grid; grid-template-columns:max-content 1fr; gap:4px 16px; margin:0 0 22px; font-size:14px; }
.meta dt { color:var(--muted); } .meta dd { margin:0; overflow-wrap:anywhere; }
code { font:13px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace; background:var(--code); padding:1px 5px; border-radius:4px; }
.stats { display:grid; grid-template-columns:repeat(auto-fit,minmax(130px,1fr)); gap:10px; margin:0 0 16px; }
.stat { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:12px 14px; }
.stat .n { display:block; font-size:22px; font-weight:650; font-variant-numeric:tabular-nums; }
.stat .l { color:var(--muted); font-size:13px; }
.stat.bad { border-color:var(--bad); } .stat.bad .n { color:var(--bad); }
.note { color:var(--muted); font-size:14px; max-width:75ch; margin:0 0 22px; }
.scroll { overflow-x:auto; border:1px solid var(--line); border-radius:10px; background:var(--card); }
table { border-collapse:collapse; width:100%; font-size:14px; }
th, td { padding:8px 12px; text-align:right; border-bottom:1px solid var(--line); white-space:nowrap; font-variant-numeric:tabular-nums; }
th { color:var(--muted); font-weight:550; font-size:13px; }
th:nth-child(2), td:nth-child(2), th:first-child, td:first-child, th:last-child, td:last-child { text-align:left; }
tbody tr:last-child td { border-bottom:0; }
tr.row-bad { background:var(--badbg); }
td.bad { color:var(--bad); font-weight:650; }
.dot { font-weight:700; } .dot.ok { color:var(--ok); } .dot.bad { color:var(--bad); }
.child { color:var(--muted); margin-right:2px; }
.muted { color:var(--muted); }
details { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:10px 16px; margin:0 0 10px; }
summary { cursor:pointer; font-weight:600; }
ul { margin:4px 0 8px; padding-left:20px; } li { margin:2px 0; overflow-wrap:anywhere; }
.count { display:inline-block; min-width:2.5em; font-variant-numeric:tabular-nums; color:var(--muted); }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta<'static> {
        Meta { mapping: Path::new("m.toml"), dump: Path::new("dump"), db: "host=db user=app password=hunter2" }
    }

    #[test]
    fn password_never_reaches_the_report() {
        let html = render(&[], &meta(), "now");
        assert!(!html.contains("hunter2"));
        assert_eq!(redact("postgres://app:hunter2@db:5432/shop"), "postgres://app@db:5432/shop");
    }

    #[test]
    fn failing_table_flips_the_verdict_and_is_escaped() {
        let bad = Check {
            table: "<orders>".into(),
            changed: 1,
            changed_columns: vec![("status".into(), 1)],
            ..Default::default()
        };
        let good = Check { table: "customers".into(), ..Default::default() };
        let html = render(&[good, bad], &meta(), "now");
        assert!(html.contains(">FAIL<"));
        assert!(html.contains("1 of 2 tables don't match"));
        assert!(html.contains("&lt;orders&gt;") && !html.contains("<orders>"));
    }

    #[test]
    fn all_matching_passes() {
        let html = render(&[Check { table: "customers".into(), ..Default::default() }], &meta(), "now");
        assert!(html.contains(">PASS<") && html.contains("all 1 tables match"));
    }

    #[test]
    fn date_is_formatted() {
        assert!(now_utc().ends_with(" UTC"));
        assert_eq!(now_utc().len(), "2026-11-08 14:05 UTC".len());
    }
}
