// Throwaway prototype for Silk Setup (docs/spec/silk-setup.md, step 1).
// Question: can one prebuilt exe carry a payload appended to its own end, and unpack it?
//
//   silk-proto.exe pack <payload_dir> <out.exe>   copy this exe, append the payload
//   <out.exe> [target_dir]                        unpack the appended payload
//
// Layout of a packed exe: [stub exe][archive][archive offset: u64 LE][MAGIC: 8 bytes]
// Archive: u32 file count, then per file: u16 path len, path (UTF-8, '/'), u64 len, bytes.
// ponytail: whole files held in memory, uncompressed; stream + compress for real payloads.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

const MAGIC: &[u8; 8] = b"SILKPAK1";
const TRAILER: usize = 16;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = if args.len() == 4 && args[1] == "pack" {
        pack(Path::new(&args[2]), Path::new(&args[3]))
    } else {
        unpack(args.get(1).map(PathBuf::from))
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn pack(payload: &Path, out: &Path) -> io::Result<()> {
    let me = fs::read(env::current_exe()?)?;
    // A packed exe used as the stub would nest payloads; use only the bare stub.
    // ponytail: a signed bare stub keeps its signature bytes here; real tool strips or re-signs.
    let stub = &me[..stub_len(&me).map_or(me.len(), |(start, _)| start)];

    let mut files = Vec::new();
    walk(payload, payload, &mut files)?;
    let mut archive = Vec::new();
    archive.extend((files.len() as u32).to_le_bytes());
    for (rel, full) in &files {
        let data = fs::read(full)?;
        archive.extend((rel.len() as u16).to_le_bytes());
        archive.extend(rel.as_bytes());
        archive.extend((data.len() as u64).to_le_bytes());
        archive.extend(&data);
        println!("  + {rel} ({} bytes)", data.len());
    }

    let mut f = fs::File::create(out)?;
    f.write_all(stub)?;
    f.write_all(&archive)?;
    f.write_all(&(stub.len() as u64).to_le_bytes())?;
    f.write_all(MAGIC)?;
    println!("packed {} files into {} ({} byte stub + {} byte archive)",
        files.len(), out.display(), stub.len(), archive.len());
    Ok(())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Offset where the archive starts, if this file carries one.
fn stub_len(bytes: &[u8]) -> Option<(usize, usize)> {
    let end = signature_start(bytes).unwrap_or(bytes.len());
    // Signing pads the file to 8 bytes before the signature, so the trailer may sit a little earlier.
    (0..8).filter_map(|pad| end.checked_sub(pad)).find_map(|end| {
        if end < TRAILER || &bytes[end - 8..end] != MAGIC {
            return None;
        }
        let at = end - TRAILER;
        let start = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        (start <= at).then_some((start, at))
    })
}

/// File offset of the Authenticode signature, which Windows appends after our trailer.
/// Read from the PE header's security data directory (entry 4).
fn signature_start(b: &[u8]) -> Option<usize> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize);
    let pe = u32_at(0x3c)?;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let opt = pe + 24;
    let dirs = match u16_at(opt)? { 0x10b => opt + 96, 0x20b => opt + 112, _ => return None };
    let (offset, size) = (u32_at(dirs + 4 * 8)?, u32_at(dirs + 4 * 8 + 4)?);
    (size > 0 && offset + size == b.len()).then_some(offset)
}

fn unpack(target: Option<PathBuf>) -> io::Result<()> {
    let exe = env::current_exe()?;
    let bytes = fs::read(&exe)?;
    let Some((start, archive_end)) = stub_len(&bytes) else {
        println!("No payload in this exe. Usage: silk-proto.exe pack <payload_dir> <out.exe>");
        return Ok(());
    };
    let target = target.unwrap_or_else(|| exe.with_file_name(format!(
        "{}-unpacked", exe.file_stem().unwrap().to_string_lossy())));

    let bad = || io::Error::new(io::ErrorKind::InvalidData, "archive is corrupt");
    let mut r = Reader { buf: &bytes[start..archive_end] };
    let count = u32::from_le_bytes(r.take(4).ok_or_else(bad)?.try_into().unwrap());
    for _ in 0..count {
        let n = u16::from_le_bytes(r.take(2).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let rel = std::str::from_utf8(r.take(n).ok_or_else(bad)?).map_err(|_| bad())?;
        let len = u64::from_le_bytes(r.take(8).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let data = r.take(len).ok_or_else(bad)?;
        // Refuse paths that would escape the target directory.
        let rel_path = Path::new(rel);
        if !rel_path.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsafe path: {rel}")));
        }
        let dest = target.join(rel_path);
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::write(&dest, data)?;
        println!("  - {rel} ({len} bytes)");
    }
    println!("unpacked {count} files to {}", target.display());
    Ok(())
}

struct Reader<'a> { buf: &'a [u8] }
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.buf.len() { return None; }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Some(head)
    }
}
