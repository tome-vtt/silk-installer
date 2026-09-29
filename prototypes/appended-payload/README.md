# Prototype: appended payload

Throwaway. Answers one question: can a single prebuilt exe carry files appended to its own end,
unpack them, and still do so after Authenticode signing? Yes (see `docs/spec.md` § 10).

```powershell
cargo build --release
.\target\release\silk-proto.exe pack <payload_dir> my-installer.exe
.\my-installer.exe <target_dir>
```
