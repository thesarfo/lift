mod convert;
mod dump;
mod ids;
mod load;
mod mapping;
mod plan;
mod report;
mod schema;
mod verify;

use std::{collections::BTreeMap, env, path::PathBuf, process};

use anyhow::{Context, Result, bail};
use postgres::{Client, NoTls};

const USAGE: &str = "\
lift: move a mongodump into a Postgres schema you already designed

usage:
  lift plan   DUMP --schema SCHEMA     draft a mapping from the dump and the target schema
  lift check  MAPPING --dump DIR       load everything, report, then roll back
  lift run    MAPPING --dump DIR       load for real (safe to re-run)
  lift verify MAPPING --dump DIR       is every document in its table?

options:
  --db URL        Postgres connection string (or set LIFT_DATABASE_URL)
  --rejects DIR   where rejected documents go (default: lift-rejects)
  --report FILE   verify only: also write an HTML report for sign-off
  --schema NAME   plan only: the target schema to read
  -o FILE         plan only: write the draft here instead of stdout
";

struct Args {
    cmd: String,
    mapping: Option<PathBuf>,
    dump: PathBuf,
    db: String,
    rejects: PathBuf,
    report: Option<PathBuf>,
    schema: Option<String>,
    out: Option<PathBuf>,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("lift: {e:#}");
        process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let Some(args) = parse()? else {
        print!("{USAGE}");
        return Ok(());
    };
    if args.report.is_some() && args.cmd != "verify" {
        bail!("--report only works with verify");
    }
    let mut db = Client::connect(&args.db, NoTls).context("connect to postgres")?;

    if args.cmd == "plan" {
        let schema_name = args.schema.context("missing --schema NAME")?;
        let tables = schema::read(&mut db, &schema_name)?;
        let draft = plan::build(&schema_name, &args.dump, &tables)?;
        let text = plan::render(&draft);
        match &args.out {
            Some(path) => {
                std::fs::write(path, &text).with_context(|| format!("write {}", path.display()))?;
                eprintln!("draft mapping written to {}", path.display());
            }
            None => print!("{text}"),
        }
        return Ok(());
    }

    let m = mapping::load(args.mapping.as_deref().context("missing MAPPING file")?)?;

    match args.cmd.as_str() {
        "check" | "run" => {
            let commit = args.cmd == "run";
            let opts = load::Options { dump: &args.dump, rejects: &args.rejects, commit };
            let reports = load::run(&mut db, &m, &opts)?;
            print_reports(&reports, &args.rejects);
            if !commit {
                println!("\ncheck only: rolled back, nothing was written");
            }
        }
        "verify" => {
            let checks = verify::run(&mut db, &m, &args.dump)?;
            print_checks(&checks);
            if let Some(path) = &args.report {
                report::write(
                    path,
                    &checks,
                    &report::Meta { mapping: args.mapping.as_deref().unwrap(), dump: &args.dump, db: &args.db },
                )?;
                println!("\nreport written to {}", path.display());
            }
            let bad: Vec<&str> = checks.iter().filter(|c| !c.ok()).map(|c| c.table.as_str()).collect();
            if !bad.is_empty() {
                bail!("{} table(s) don't match the dump: {}", bad.len(), bad.join(", "));
            }
            println!("\nevery table matches the dump (rejected documents are listed in {})", args.rejects.display());
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn print_reports(reports: &[load::Report], rejects: &std::path::Path) {
    println!("{:<34}{:>10}{:>10}{:>10}{:>9}", "table", "docs", "loaded", "rejected", "seconds");
    for r in reports {
        let note = if r.missing { "  (not in dump)" } else { "" };
        println!(
            "{:<34}{:>10}{:>10}{:>10}{:>9.1}{note}",
            r.table,
            r.docs,
            r.loaded,
            r.rejects.len(),
            r.elapsed.as_secs_f64()
        );
        let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
        for rej in &r.rejects {
            *reasons.entry(rej.reason.as_str()).or_default() += 1;
        }
        for (reason, n) in reasons.iter().take(5) {
            println!("    {n} x {reason}");
        }
        if reasons.len() > 5 {
            println!("    ... {} more reasons", reasons.len() - 5);
        }
        for rej in r.nulled_refs.iter().take(3) {
            println!("    {} -> {}", rej.id, rej.reason);
        }
        if r.nulled_refs.len() > 3 {
            println!("    ... {} more references left NULL", r.nulled_refs.len() - 3);
        }
        for (col, n) in &r.counts.zero_ids {
            println!("    {n} x {col} was Go's zero id, stored NULL");
        }
        for (col, n) in &r.counts.zero_filled {
            println!("    {n} x {col} filled with Go's zero value");
        }
    }
    if reports.iter().any(|r| !r.rejects.is_empty() || !r.nulled_refs.is_empty()) {
        println!("\nrejected documents are in {}", rejects.display());
    }
}

fn print_checks(checks: &[verify::Check]) {
    println!(
        "{:<34}{:>10}{:>10}{:>10}{:>9}{:>9}{:>7}{:>9}  fingerprint",
        "table", "docs", "rejected", "expected", "missing", "changed", "extra", "seconds"
    );
    for c in checks {
        let fp = if c.not_in_dump { "(not in dump)".to_string() } else { c.fingerprint.chars().take(12).collect() };
        println!(
            "{:<34}{:>10}{:>10}{:>10}{:>9}{:>9}{:>7}{:>9.1}  {fp}",
            c.table,
            c.docs,
            c.rejected,
            c.expected,
            c.missing,
            c.changed,
            c.extra,
            c.elapsed.as_secs_f64()
        );
        for (col, n) in &c.changed_columns {
            println!("    {col} differs on {n} row(s)");
        }
        for s in &c.samples {
            println!("    {s}");
        }
    }
}

fn parse() -> Result<Option<Args>> {
    let mut it = env::args().skip(1);
    let Some(cmd) = it.next() else { return Ok(None) };
    if !matches!(cmd.as_str(), "plan" | "check" | "run" | "verify") {
        if matches!(cmd.as_str(), "-h" | "--help" | "help") {
            return Ok(None);
        }
        bail!("unknown command {cmd:?}\n\n{USAGE}");
    }
    let mut mapping = None;
    let mut dump = None;
    let mut db = env::var("LIFT_DATABASE_URL").ok();
    let mut rejects = PathBuf::from("lift-rejects");
    let mut report = None;
    let mut schema = None;
    let mut out = None;
    while let Some(a) = it.next() {
        let mut value = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--dump" => dump = Some(PathBuf::from(value()?)),
            "--db" => db = Some(value()?),
            "--rejects" => rejects = PathBuf::from(value()?),
            "--report" => report = Some(PathBuf::from(value()?)),
            "--schema" => schema = Some(value()?),
            "-o" | "--out" => out = Some(PathBuf::from(value()?)),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ if cmd == "plan" && dump.is_none() => dump = Some(PathBuf::from(&a)),
            _ if mapping.is_none() => mapping = Some(PathBuf::from(&a)),
            _ => bail!("unexpected argument {a:?}"),
        }
    }
    Ok(Some(Args {
        cmd,
        mapping,
        dump: dump.context("missing DUMP (or --dump DIR)")?,
        db: db.context("missing --db URL (or LIFT_DATABASE_URL)")?,
        rejects,
        report,
        schema,
        out,
    }))
}
