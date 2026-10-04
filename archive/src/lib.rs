// The archive a Silk installer carries (docs/spec.md § 3, § 10).
//
// Layout: [stub exe][zip][archive start offset: u64 LE][MAGIC: 8 bytes][signing padding + signature?]
// Zip entries: payload/... (installed files), uninstaller.exe (installers only), theme/... (the theme's folder,
// optional), config.toml (the resolved config).

pub mod config;

use config::Config;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MAGIC: &[u8; 8] = b"SILKPAK1";
const TRAILER: u64 = 16;
const PAYLOAD: &str = "payload/";
const THEME: &str = "theme/";
const CONFIG: &str = "config.toml";
const UNINSTALLER: &str = "uninstaller.exe";

/// Copy `stub`, append a zip, then the trailer, writing `out`. The zip holds `config`, `theme_dir` (under
/// `theme/`; it must hold an `index.html`) and, for an installer, `payload` = (the folder to install, under
/// `payload/`; the uninstaller exe, as `uninstaller.exe`). `None` builds that uninstaller: no payload, so
/// it can be signed before the installer packs it.
pub fn build(
    stub: &Path,
    payload: Option<(&Path, &Path)>,
    theme_dir: Option<&Path>,
    config: &Config,
    out: &Path,
) -> io::Result<()> {
    check_stub(stub)?;
    let mut files = Vec::new();
    if let Some((dir, uninstaller)) = payload {
        walk(dir, dir, PAYLOAD, &mut files)?;
        files.push((UNINSTALLER.into(), uninstaller.to_path_buf()));
    }
    if let Some(theme) = theme_dir {
        if !theme.join("index.html").is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("theme {} has no index.html", theme.display()),
            ));
        }
        walk(theme, theme, THEME, &mut files)?;
    }
    files.sort();

    let mut f = File::create(out)?;
    let start = io::copy(&mut File::open(stub)?, &mut f)?;
    // Stream mode: entry offsets count from the zip's own start, not the file's.
    let mut zip = ZipWriter::new_stream(BufWriter::new(&mut f));
    for (name, full) in &files {
        let mut data = File::open(full)?;
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(data.metadata()?.len() >= u32::MAX as u64);
        zip.start_file(name, opts)?;
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} already carries an archive or a signature: the stub must be the unsigned bare runtime \
             (sign the finished installer instead)",
                stub.display()
            ),
        ));
    }
    Ok(())
}

/// Every file under `dir`, as (`prefix` + its path from `root` with `/` separators, full path).
fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(root, &path, prefix, out)?;
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((format!("{prefix}{rel}"), path));
        }
    }
    Ok(())
}

pub struct Archive {
    zip: ZipArchive<BufReader<Window>>,
    payload_len: usize,
    payload_size: u64,
    uninstaller_size: u64,
}

impl Archive {
    /// Open the archive carried by `exe`; Ok(None) when the exe carries none.
    pub fn open(exe: &Path) -> io::Result<Option<Archive>> {
        let mut file = File::open(exe)?;
        let Some((start, end)) = locate(&mut file)? else { return Ok(None) };
        let mut zip = ZipArchive::new(BufReader::new(Window { file, start, end, pos: 0 }))?;
        let (mut payload_len, mut payload_size, mut uninstaller_size) = (0, 0, 0);
        for i in 0..zip.len() {
            let entry = zip.by_index_raw(i)?;
            if entry.name().starts_with(PAYLOAD) && !entry.is_dir() {
                payload_len += 1;
                payload_size += entry.size();
            } else if entry.name() == UNINSTALLER {
                uninstaller_size = entry.size();
            }
        }
        Ok(Some(Archive { zip, payload_len, payload_size, uninstaller_size }))
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

    /// Uncompressed bytes under `payload/`.
    pub fn payload_size(&self) -> u64 {
        self.payload_size
    }

    /// Size of the uninstaller an installer carries; 0 in the uninstaller itself.
    pub fn uninstaller_size(&self) -> u64 {
        self.uninstaller_size
    }

    /// Write the carried uninstaller to `dest` byte for byte, signature included; `NotFound` when there is none.
    pub fn extract_uninstaller(&mut self, dest: &Path) -> io::Result<()> {
        let mut entry = match self.zip.by_name(UNINSTALLER) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no uninstaller in this archive"));
            }
            Err(e) => return Err(e.into()),
        };
        fs::create_dir_all(dest.parent().unwrap_or(Path::new("")))?;
        io::copy(&mut entry, &mut File::create(dest)?).map(drop)
    }

    /// Whether the compiler packed a theme.
    pub fn has_theme(&self) -> bool {
        self.zip.index_for_name(&format!("{THEME}index.html")).is_some()
    }

    /// The theme file at `path` (relative to the theme folder, `/` separators, matched exactly); None when absent.
    pub fn theme_file(&mut self, path: &str) -> io::Result<Option<Vec<u8>>> {
        let mut entry = match self.zip.by_name(&format!("{THEME}{path}")) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        Ok(Some(data))
    }

    /// Extract every `payload/` file into `target`, calling `progress(files_done, bytes_done, relative_path)`
    /// after each (totals: `payload_len`, `payload_size`); stop with `Interrupted` when it returns false.
    // ponytail: progress per file, so one huge file shows no movement until it is written; count inside io::copy if that matters.
    pub fn extract_payload(
        &mut self,
        target: &Path,
        mut progress: impl FnMut(usize, u64, &str) -> bool,
    ) -> io::Result<()> {
        let (mut done, mut bytes) = (0, 0);
        for i in 0..self.zip.len() {
            let mut entry = self.zip.by_index(i)?;
            let Some(rel) = entry.name().strip_prefix(PAYLOAD).map(str::to_owned) else { continue };
            if entry.is_dir() {
                continue;
            }
            // enclosed_name normalises `..`; what is left must still sit under payload/.
            let dest = entry
                .enclosed_name()
                .and_then(|p| Some(target.join(p.strip_prefix("payload").ok()?)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unsafe path: {}", entry.name())))?;
            fs::create_dir_all(dest.parent().unwrap())?;
            bytes += io::copy(&mut entry, &mut File::create(&dest)?)?;
            done += 1;
            if !progress(done, bytes, &rel) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "extraction cancelled"));
            }
        }
        Ok(())
    }
}

/// The byte range [start, end) of the file that holds the zip.
struct Window {
    file: File,
    start: u64,
    end: u64,
    pos: u64,
}

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
    let end = match signature_start(f)? {
        Some(at) => at,
        None => f.metadata()?.len(),
    };
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
    let at = || {
        let entry = security_entry(&b)?;
        let (offset, size) = (u32_at(&b, entry)?, u32_at(&b, entry + 4)?);
        (size > 0 && offset + size == len).then_some(offset)
    };
    Ok(at())
}

fn u32_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as u64)
}

/// Where the PE header in `b` keeps the security data directory entry (offset, size).
fn security_entry(b: &[u8]) -> Option<usize> {
    let pe = u32_at(b, 0x3c)? as usize;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let opt = pe + 24;
    let dirs = match u16::from_le_bytes(b.get(opt..opt + 2)?.try_into().ok()?) {
        0x10b => opt + 96,
        0x20b => opt + 112,
        _ => return None,
    };
    Some(dirs + 4 * 8)
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

    /// A stand-in for the uninstaller `build(stub, None, ..)` makes, in `dir/uninstall.exe`.
    fn fake_uninstaller(dir: &Path) -> PathBuf {
        let u = dir.join("uninstall.exe");
        fs::write(&u, b"MZ fake uninstaller").unwrap();
        u
    }

    /// Stub + payload built into `dir/out.exe`.
    fn built(dir: &Path) -> (PathBuf, Vec<(&'static str, Vec<u8>)>) {
        let stub = dir.join("stub.exe");
        fs::write(&stub, fake_stub()).unwrap();
        let files = payload(&dir.join("payload"));
        let out = dir.join("out.exe");
        build(&stub, Some((&dir.join("payload"), &fake_uninstaller(dir))), None, &sample_config(), &out).unwrap();
        (out, files)
    }

    /// What signtool does: pad to 8 bytes, append the signature, point directory entry 4 at it.
    fn sign(exe: &Path) {
        let mut b = fs::read(exe).unwrap();
        b.resize(b.len().next_multiple_of(8), 0);
        let (offset, size) = (b.len() as u32, 64u32);
        b.extend([0xAB; 64]);
        b[0x128..0x12c].copy_from_slice(&offset.to_le_bytes());
        b[0x12c..0x130].copy_from_slice(&size.to_le_bytes());
        fs::write(exe, &b).unwrap();
    }

    fn extract_and_check(exe: &Path, target: &Path, files: &[(&str, Vec<u8>)]) {
        let mut a = Archive::open(exe).unwrap().expect("archive found");
        assert_eq!(a.payload_len(), files.len());
        assert_eq!(a.payload_size(), files.iter().map(|(_, d)| d.len() as u64).sum::<u64>());
        let mut seen = Vec::new();
        a.extract_payload(target, |done, bytes, rel| {
            seen.push((done, bytes, rel.to_string()));
            true
        })
        .unwrap();
        assert_eq!(seen.len(), files.len());
        assert_eq!(seen.last().unwrap().0, files.len());
        assert_eq!(seen.last().unwrap().1, a.payload_size());
        assert!(seen.windows(2).all(|w| w[0].1 <= w[1].1), "bytes only grow");
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
    fn theme_is_packed_beside_the_payload() {
        let dir = tmp("theme");
        let stub = dir.join("stub.exe");
        fs::write(&stub, fake_stub()).unwrap();
        let files = payload(&dir.join("payload"));
        let theme = dir.join("theme");
        fs::create_dir_all(theme.join("img")).unwrap();
        fs::write(theme.join("index.html"), "<h1>hi</h1>").unwrap();
        fs::write(theme.join("img/logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let out = dir.join("out.exe");
        let uninstaller = fake_uninstaller(&dir);
        build(&stub, Some((&dir.join("payload"), &uninstaller)), Some(&theme), &sample_config(), &out).unwrap();

        let mut a = Archive::open(&out).unwrap().unwrap();
        assert!(a.has_theme());
        assert_eq!(a.theme_file("index.html").unwrap().unwrap(), b"<h1>hi</h1>");
        assert_eq!(a.theme_file("img/logo.png").unwrap().unwrap(), [0x89, b'P', b'N', b'G']);
        assert_eq!(a.theme_file("img/missing.png").unwrap(), None);
        // Theme files are not installed, and the theme cannot reach other entries.
        assert_eq!(a.payload_len(), files.len());
        assert_eq!(a.theme_file("../config.toml").unwrap(), None);
        extract_and_check(&out, &dir.join("target"), &files);
        assert!(!dir.join("target/index.html").exists());

        let (plain, _) = built(&dir);
        assert!(!Archive::open(&plain).unwrap().unwrap().has_theme());

        // The example theme stays packable.
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/themes/minimal");
        build(&stub, Some((&dir.join("payload"), &uninstaller)), Some(&example), &sample_config(), &out).unwrap();
        assert!(Archive::open(&out).unwrap().unwrap().has_theme());
    }

    #[test]
    fn theme_without_index_html_is_refused() {
        let dir = tmp("theme-no-index");
        let stub = dir.join("stub.exe");
        fs::write(&stub, fake_stub()).unwrap();
        payload(&dir.join("payload"));
        let theme = dir.join("theme");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("main.html"), "x").unwrap();
        for t in [theme.clone(), dir.join("no-such-theme")] {
            let err = build(&stub, None, Some(&t), &sample_config(), &dir.join("out.exe")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("has no index.html"), "{err}");
        }
    }

    #[test]
    fn signed_layout() {
        let dir = tmp("signed");
        let (out, files) = built(&dir);
        sign(&out);
        extract_and_check(&out, &dir.join("target"), &files);
    }

    #[test]
    fn installer_carries_the_signed_uninstaller_which_carries_no_payload() {
        let dir = tmp("uninstaller");
        let stub = dir.join("stub.exe");
        fs::write(&stub, fake_stub()).unwrap();
        let files = payload(&dir.join("payload"));
        let theme = dir.join("theme");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("index.html"), "<h1>bye</h1>").unwrap();

        // The uninstaller: stub, theme and config, no payload; signed before it is packed.
        let uninstaller = dir.join("uninstall.exe");
        build(&stub, None, Some(&theme), &sample_config(), &uninstaller).unwrap();
        sign(&uninstaller);
        let mut u = Archive::open(&uninstaller).unwrap().expect("archive found");
        assert_eq!((u.payload_len(), u.payload_size(), u.uninstaller_size()), (0, 0, 0));
        assert_eq!(u.theme_file("index.html").unwrap().unwrap(), b"<h1>bye</h1>");
        assert_eq!(format!("{:?}", u.config().unwrap()), format!("{:?}", sample_config()));
        assert_eq!(u.extract_uninstaller(&dir.join("none.exe")).unwrap_err().kind(), io::ErrorKind::NotFound);

        // The installer carries it byte for byte, signature included, beside (not in) the payload.
        let setup = dir.join("setup.exe");
        build(&stub, Some((&dir.join("payload"), &uninstaller)), Some(&theme), &sample_config(), &setup).unwrap();
        sign(&setup);
        let signed = fs::read(&uninstaller).unwrap();
        let mut a = Archive::open(&setup).unwrap().unwrap();
        assert_eq!(a.uninstaller_size(), signed.len() as u64);
        a.extract_uninstaller(&dir.join("out/Uninstall Sample.exe")).unwrap();
        assert_eq!(fs::read(dir.join("out/Uninstall Sample.exe")).unwrap(), signed);
        extract_and_check(&setup, &dir.join("target"), &files);
        assert!(!dir.join("target/uninstaller.exe").exists());
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
        let err = build(&out, None, None, &sample_config(), &again).unwrap_err();
        assert!(err.to_string().contains("unsigned bare runtime"), "{err}");

        let mut b = fake_stub();
        let offset = b.len() as u32;
        b.extend([0xAB; 64]);
        b[0x128..0x12c].copy_from_slice(&offset.to_le_bytes());
        b[0x12c..0x130].copy_from_slice(&64u32.to_le_bytes());
        let signed = dir.join("signed-stub.exe");
        fs::write(&signed, b).unwrap();
        let err = build(&signed, None, None, &sample_config(), &again).unwrap_err();
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
