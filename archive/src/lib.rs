// The archive a Silk installer carries (docs/spec.md § 3, § 10).
//
// Layout: [stub exe][zip][archive start offset: u64 LE][MAGIC: 8 bytes][signing padding + signature?]
// Zip entries: payload/... (installed files), config.toml (the resolved config); theme/... comes later.

pub mod config;

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use config::Config;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MAGIC: &[u8; 8] = b"SILKPAK1";
const TRAILER: u64 = 16;
const PAYLOAD: &str = "payload/";
const CONFIG: &str = "config.toml";

/// Copy `stub`, append `config` and `payload_dir` (under `payload/`) as a zip, then the trailer, writing `out`.
pub fn build(stub: &Path, payload_dir: &Path, config: &Config, out: &Path) -> io::Result<()> {
    check_stub(stub)?;
    let mut src = File::open(stub)?;
    let mut files = Vec::new();
    walk(payload_dir, payload_dir, &mut files)?;
    files.sort();

    let mut f = File::create(out)?;
    src.seek(SeekFrom::Start(0))?;
    let start = io::copy(&mut src, &mut f)?;
    // Stream mode: entry offsets count from the zip's own start, not the file's.
    let mut zip = ZipWriter::new_stream(BufWriter::new(&mut f));
    for (rel, full) in &files {
        let mut data = File::open(full)?;
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(data.metadata()?.len() >= u32::MAX as u64);
        zip.start_file(format!("{PAYLOAD}{rel}"), opts)?;
        io::copy(&mut data, &mut zip)?;
    }
    zip.start_file(CONFIG, SimpleFileOptions::default())?;
    zip.write_all(toml::to_string(config).map_err(io::Error::other)?.as_bytes())?;
    zip.finish()?.into_inner().into_inner().map_err(|e| e.into_error())?;
    f.write_all(&start.to_le_bytes())?;
    f.write_all(MAGIC)
}

/// Refuse a stub that already carries an archive or a signature.
pub fn check_stub(stub: &Path) -> io::Result<()> {
    let mut f = File::open(stub)?;
    if signature_start(&mut f)?.is_some() || locate(&mut f)?.is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!(
            "{} already carries an archive or a signature: the stub must be the unsigned bare runtime \
             (sign the finished installer instead)", stub.display())));
    }
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

pub struct Archive {
    zip: ZipArchive<BufReader<Window>>,
    payload_len: usize,
}

impl Archive {
    /// Open the archive carried by `exe`; Ok(None) when the exe carries none.
    pub fn open(exe: &Path) -> io::Result<Option<Archive>> {
        let mut file = File::open(exe)?;
        let Some((start, end)) = locate(&mut file)? else { return Ok(None) };
        let zip = ZipArchive::new(BufReader::new(Window { file, start, end, pos: 0 }))?;
        let payload_len = zip.file_names().filter(|n| n.starts_with(PAYLOAD) && !n.ends_with('/')).count();
        Ok(Some(Archive { zip, payload_len }))
    }

    /// The config the compiler stored.
    pub fn config(&mut self) -> io::Result<Config> {
        let mut text = String::new();
        self.zip.by_name(CONFIG)?.read_to_string(&mut text)?;
        config::parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{CONFIG}: {e}")))
    }

    /// Number of files under `payload/`.
    pub fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Extract every `payload/` file into `target`, calling `progress(done, total, relative_path)` after
    /// each; stop with `Interrupted` when it returns false.
    pub fn extract_payload(&mut self, target: &Path, mut progress: impl FnMut(usize, usize, &str) -> bool) -> io::Result<()> {
        let mut done = 0;
        for i in 0..self.zip.len() {
            let mut entry = self.zip.by_index(i)?;
            let Some(rel) = entry.name().strip_prefix(PAYLOAD).map(str::to_owned) else { continue };
            if entry.is_dir() {
                continue;
            }
            // enclosed_name normalises `..`; what is left must still sit under payload/.
            let dest = entry.enclosed_name()
                .and_then(|p| Some(target.join(p.strip_prefix("payload").ok()?)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unsafe path: {}", entry.name())))?;
            fs::create_dir_all(dest.parent().unwrap())?;
            io::copy(&mut entry, &mut File::create(&dest)?)?;
            done += 1;
            if !progress(done, self.payload_len, &rel) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "extraction cancelled"));
            }
        }
        Ok(())
    }
}

/// The byte range [start, end) of the file that holds the zip.
struct Window { file: File, start: u64, end: u64, pos: u64 }

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = (self.end - self.start).saturating_sub(self.pos);
        let n = buf.len().min(left as usize);
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.file.read(&mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Window {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
            SeekFrom::End(n) => (self.end - self.start).checked_add_signed(n),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

/// Where the zip sits, if this file carries one: (start, end) offsets.
fn locate(f: &mut File) -> io::Result<Option<(u64, u64)>> {
    let end = match signature_start(f)? { Some(at) => at, None => f.metadata()?.len() };
    // Signing pads the file to 8 bytes before the signature, so the trailer may sit up to 7 bytes earlier.
    let from = end.saturating_sub(TRAILER + 7);
    let mut tail = Vec::new();
    f.seek(SeekFrom::Start(from))?;
    f.take(end - from).read_to_end(&mut tail)?;
    Ok((0..8).filter_map(|pad| tail.len().checked_sub(pad)).find_map(|e| {
        if e < TRAILER as usize || &tail[e - 8..e] != MAGIC {
            return None;
        }
        let at = from + e as u64 - TRAILER;
        let start = u64::from_le_bytes(tail[e - 16..e - 8].try_into().unwrap());
        (start <= at).then_some((start, at))
    }))
}

/// File offset of the Authenticode signature, which Windows appends after our trailer.
/// Read from the PE header's security data directory (entry 4).
// ponytail: only looks at the first 4 KiB for the PE header; linkers put it well inside that.
fn signature_start(f: &mut File) -> io::Result<Option<u64>> {
    let len = f.metadata()?.len();
    let mut b = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.take(4096).read_to_end(&mut b)?;
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as u64);
    let at = || {
        let pe = u32_at(0x3c)? as usize;
        if b.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let opt = pe + 24;
        let dirs = match u16_at(opt)? { 0x10b => opt + 96, 0x20b => opt + 112, _ => return None };
        let (offset, size) = (u32_at(dirs + 4 * 8)?, u32_at(dirs + 4 * 8 + 4)?);
        (size > 0 && offset + size == len).then_some(offset)
    };
    Ok(at())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silk-archive-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Minimal PE: MZ, e_lfanew = 0x80, "PE\0\0", PE32+ optional header; security directory at 0x128.
    fn fake_stub() -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        b.extend(b"stub code");
        b
    }

    fn payload(dir: &Path) -> Vec<(&'static str, Vec<u8>)> {
        let noise: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let files = vec![
            ("app.exe", noise),
            ("readme.txt", b"hello".to_vec()),
            ("lib/deep/data.bin", vec![7u8; 10_000]),
            ("empty", vec![]),
        ];
        for (rel, data) in &files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        files
    }

    fn sample_config() -> Config {
        config::parse(include_str!("../../examples/spider.toml")).unwrap()
    }

    /// Stub + payload built into `dir/out.exe`.
    fn built(dir: &Path) -> (PathBuf, Vec<(&'static str, Vec<u8>)>) {
        let stub = dir.join("stub.exe");
        fs::write(&stub, fake_stub()).unwrap();
        let files = payload(&dir.join("payload"));
        let out = dir.join("out.exe");
        build(&stub, &dir.join("payload"), &sample_config(), &out).unwrap();
        (out, files)
    }

    fn extract_and_check(exe: &Path, target: &Path, files: &[(&str, Vec<u8>)]) {
        let mut a = Archive::open(exe).unwrap().expect("archive found");
        assert_eq!(a.payload_len(), files.len());
        let mut seen = Vec::new();
        a.extract_payload(target, |done, total, rel| {
            assert_eq!(total, files.len());
            seen.push((done, rel.to_string()));
            true
        })
        .unwrap();
        assert_eq!(seen.len(), files.len());
        assert_eq!(seen.last().unwrap().0, files.len());
        for (rel, data) in files {
            assert_eq!(&fs::read(target.join(rel)).unwrap(), data, "{rel}");
        }
    }

    #[test]
    fn round_trip() {
        let dir = tmp("round-trip");
        let (out, files) = built(&dir);
        assert!(fs::read(&out).unwrap().starts_with(&fake_stub()));
        extract_and_check(&out, &dir.join("target"), &files);
        let config = Archive::open(&out).unwrap().unwrap().config().unwrap();
        assert_eq!(format!("{config:?}"), format!("{:?}", sample_config()));
    }

    #[test]
    fn signed_layout() {
        let dir = tmp("signed");
        let (out, files) = built(&dir);
        // What signtool does: pad to 8 bytes, append the signature, point directory entry 4 at it.
        let mut b = fs::read(&out).unwrap();
        b.resize(b.len().next_multiple_of(8), 0);
        let (offset, size) = (b.len() as u32, 64u32);
        b.extend([0xAB; 64]);
        b[0x128..0x12c].copy_from_slice(&offset.to_le_bytes());
        b[0x12c..0x130].copy_from_slice(&size.to_le_bytes());
        fs::write(&out, &b).unwrap();
        extract_and_check(&out, &dir.join("target"), &files);
    }

    #[test]
    fn no_archive() {
        let dir = tmp("none");
        let plain = dir.join("plain.exe");
        fs::write(&plain, fake_stub()).unwrap();
        assert!(Archive::open(&plain).unwrap().is_none());
        fs::write(&plain, b"").unwrap();
        assert!(Archive::open(&plain).unwrap().is_none());
    }

    #[test]
    fn corrupt() {
        let dir = tmp("corrupt");
        let (out, _) = built(&dir);
        let good = fs::read(&out).unwrap();
        let start = fake_stub().len();
        // Inside the first entry's compressed data, then the zip's end-of-central-directory signature.
        for at in [start + 200, good.len() - 16 - 22] {
            let mut b = good.clone();
            b[at] ^= 0xFF;
            fs::write(&out, &b).unwrap();
            let result = Archive::open(&out)
                .and_then(|a| a.ok_or(io::ErrorKind::NotFound.into()))
                .and_then(|mut a| a.extract_payload(&dir.join("target"), |_, _, _| true));
            assert!(result.is_err(), "flip at {at} went unnoticed");
        }
    }

    #[test]
    fn cancel() {
        let dir = tmp("cancel");
        let (out, _) = built(&dir);
        let mut a = Archive::open(&out).unwrap().unwrap();
        let err = a.extract_payload(&dir.join("target"), |done, _, _| done < 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn refuses_packed_or_signed_stub() {
        let dir = tmp("refuse");
        let (out, _) = built(&dir);
        let again = dir.join("again.exe");
        let err = build(&out, &dir.join("payload"), &sample_config(), &again).unwrap_err();
        assert!(err.to_string().contains("unsigned bare runtime"), "{err}");

        let mut b = fake_stub();
        let offset = b.len() as u32;
        b.extend([0xAB; 64]);
        b[0x128..0x12c].copy_from_slice(&offset.to_le_bytes());
        b[0x12c..0x130].copy_from_slice(&64u32.to_le_bytes());
        let signed = dir.join("signed-stub.exe");
        fs::write(&signed, b).unwrap();
        let err = build(&signed, &dir.join("payload"), &sample_config(), &again).unwrap_err();
        assert!(err.to_string().contains("unsigned bare runtime"), "{err}");
    }

    #[test]
    fn refuses_escaping_path() {
        let dir = tmp("escape");
        let exe = dir.join("evil.exe");
        let mut f = File::create(&exe).unwrap();
        f.write_all(&fake_stub()).unwrap();
        let mut zip = ZipWriter::new_stream(&mut f);
        zip.start_file("payload/../../evil.txt", SimpleFileOptions::default()).unwrap();
        zip.write_all(b"pwned").unwrap();
        zip.finish().unwrap();
        f.write_all(&(fake_stub().len() as u64).to_le_bytes()).unwrap();
        f.write_all(MAGIC).unwrap();
        drop(f);
        let mut a = Archive::open(&exe).unwrap().unwrap();
        let err = a.extract_payload(&dir.join("t/target"), |_, _, _| true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!dir.join("evil.txt").exists() && !dir.join("t/evil.txt").exists());
    }
}
