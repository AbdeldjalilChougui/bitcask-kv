use bitcask_kv::Bitcask;
use std::io::{Read, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: bitcask <dir> <command>

commands:
  get <key>            print the value (exit 1 if missing)
  put <key> [value]    store value (reads stdin if value omitted)
  del <key>            delete a key
  keys                 list keys
  merge                compact live data into new files with hints
  stats                show key/file/byte counts";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bitcask: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> std::io::Result<ExitCode> {
    let (dir, cmd, rest) = match args {
        [dir, cmd, rest @ ..] => (dir, cmd.as_str(), rest),
        _ => {
            eprintln!("{USAGE}");
            return Ok(ExitCode::from(2));
        }
    };
    let mut db = Bitcask::open(dir)?;
    let mut out = std::io::stdout().lock();
    match (cmd, rest) {
        ("get", [k]) => match db.get(k.as_bytes())? {
            Some(v) => {
                out.write_all(&v)?;
                if v.last() != Some(&b'\n') {
                    out.write_all(b"\n")?;
                }
            }
            None => return Ok(ExitCode::from(1)),
        },
        ("put", [k, v]) => {
            db.put(k.as_bytes(), v.as_bytes())?;
            db.sync()?;
        }
        ("put", [k]) => {
            let mut v = Vec::new();
            std::io::stdin().read_to_end(&mut v)?;
            db.put(k.as_bytes(), &v)?;
            db.sync()?;
        }
        ("del", [k]) => {
            if !db.delete(k.as_bytes())? {
                return Ok(ExitCode::from(1));
            }
            db.sync()?;
        }
        ("keys", []) => {
            for k in db.keys() {
                out.write_all(&k)?;
                out.write_all(b"\n")?;
            }
        }
        ("merge", []) => {
            let before = db.stats();
            db.merge()?;
            let after = db.stats();
            writeln!(out, "merged: {} -> {} bytes, {} -> {} files", before.total_bytes, after.total_bytes, before.data_files, after.data_files)?;
        }
        ("stats", []) => {
            let s = db.stats();
            writeln!(out, "keys:            {}", s.keys)?;
            writeln!(out, "data files:      {}", s.data_files)?;
            writeln!(out, "total bytes:     {}", s.total_bytes)?;
            writeln!(out, "live bytes:      {}", s.live_bytes)?;
            writeln!(out, "dead bytes:      {}", s.total_bytes.saturating_sub(s.live_bytes))?;
            writeln!(out, "recovered bytes: {}", s.recovered_bytes)?;
        }
        _ => {
            eprintln!("{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}
