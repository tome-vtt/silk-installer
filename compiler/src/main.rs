// silk build <payload_dir> <out.exe> [--stub <path>]
// Appends the payload to a copy of the runtime stub (docs/spec.md § 3).

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

const USAGE: &str = "usage: silk build <payload_dir> <out.exe> [--stub <path>]";

fn main() {
    if let Err(e) = run(env::args().skip(1).collect()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> io::Result<()> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, USAGE);
    let (payload, out, stub) = match args.as_slice() {
        [cmd, payload, out] if cmd == "build" => (payload, out, None),
        [cmd, payload, out, flag, stub] if cmd == "build" && flag == "--stub" => (payload, out, Some(PathBuf::from(stub))),
        _ => return Err(bad()),
    };
    let stub = match stub {
        Some(s) => s,
        None => env::current_exe()?.with_file_name("silk-stub.exe"),
    };
    if !stub.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("stub not found: {}", stub.display())));
    }
    silk_archive::build(&stub, payload.as_ref(), out.as_ref())?;
    // Reopen what we wrote: the count comes from the finished file.
    let archive = silk_archive::Archive::open(out.as_ref())?.expect("archive just built");
    println!("built {out}: {} files, {} bytes", archive.payload_len(), fs::metadata(out)?.len());
    Ok(())
}
