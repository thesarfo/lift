mod convert;
mod dump;
mod ids;
mod load;
mod mapping;
mod schema;
mod verify;

use std::{collections::BTreeMap, env, path::PathBuf, process};

use anyhow::{Context, Result, bail};
use postgres::{Client, NoTls};

const USAGE: &str = "\
lift: move a mongodump into a Postgres schema you already designed

usage:
  lift check  MAPPING --dump DIR    load everything, report, then roll back
  lift run    MAPPING --dump DIR    load for real (safe to re-run)
  lift verify MAPPING --dump DIR    is every document in its table?

options:
  --db URL        Postgres connection string (or set LIFT_DATABASE_URL)
  --rejects DIR   where rejected documents go (default: lift-rejects)
";

struct Args {
    cmd: String,
    mapping: PathBuf,
    dump: PathBuf,
    db: String,
    rejects: PathBuf,
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
    let m = mapping::load(&args.mapping)?;
    let mut db = Client::connect(&args.db, NoTls).context("connect to postgres")?;

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
            let mut ok = true;
            println!("{:<24}{:>8}{:>10}{:>10}{:>8}", "table", "docs", "present", "missing", "extra");
            for c in &checks {
                let missing = c.docs as i64 - c.present;
                ok &= missing == 0;
                println!("{:<24}{:>8}{:>10}{:>10}{:>8}", c.table, c.docs, c.present, missing, c.extra);
                if !c.missing.is_empty() {
                    println!("    missing: {}", c.missing.join(", "));
                }
            }
            if !ok {
                bail!("some documents are not in their tables (see {} for why)", args.rejects.display());
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn print_reports(reports: &[load::Report], rejects: &std::path::Path) {
    println!("{:<24}{:>8}{:>8}{:>10}", "table", "docs", "loaded", "rejected");
    for r in reports {
        println!("{:<24}{:>8}{:>8}{:>10}", r.table, r.docs, r.loaded, r.rejects.len());
        let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
        for rej in &r.rejects {
            *reasons.entry(rej.reason.as_str()).or_default() += 1;
        }
        for (reason, n) in reasons.iter().take(5) {
            println!("    {n} x {reason}");
        }
        for (col, n) in &r.zero_filled {
            println!("    {n} x {col} filled with Go's zero value");
        }
    }
    if reports.iter().any(|r| !r.rejects.is_empty()) {
        println!("\nrejected documents are in {}", rejects.display());
    }
}

fn parse() -> Result<Option<Args>> {
    let mut it = env::args().skip(1);
    let Some(cmd) = it.next() else { return Ok(None) };
    if !matches!(cmd.as_str(), "check" | "run" | "verify") {
        if matches!(cmd.as_str(), "-h" | "--help" | "help") {
            return Ok(None);
        }
        bail!("unknown command {cmd:?}\n\n{USAGE}");
    }
    let mut mapping = None;
    let mut dump = None;
    let mut db = env::var("LIFT_DATABASE_URL").ok();
    let mut rejects = PathBuf::from("lift-rejects");
    while let Some(a) = it.next() {
        let mut value = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--dump" => dump = Some(PathBuf::from(value()?)),
            "--db" => db = Some(value()?),
            "--rejects" => rejects = PathBuf::from(value()?),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ if mapping.is_none() => mapping = Some(PathBuf::from(&a)),
            _ => bail!("unexpected argument {a:?}"),
        }
    }
    Ok(Some(Args {
        cmd,
        mapping: mapping.context("missing MAPPING file")?,
        dump: dump.context("missing --dump DIR")?,
        db: db.context("missing --db URL (or LIFT_DATABASE_URL)")?,
        rejects,
    }))
}
