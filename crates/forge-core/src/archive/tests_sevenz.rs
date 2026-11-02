//! 7z: round trips through our writer, archives made by 7-Zip, passwords, solid blocks,
//! editing. Tests that need `7z.exe` (in `C:\Program Files\7-Zip`) skip without it.

use super::*;
use forge_jobs::{CancelToken, Progress};
use std::process::Command;

fn ctx_parts() -> (CancelToken, Progress) {
    (CancelToken::new(), Progress::default())
}

pub(super) fn seven_zip() -> Option<PathBuf> {
    let p = PathBuf::from(r"C:\Program Files\7-Zip\7z.exe");
    p.exists().then_some(p)
}

/// Run 7-Zip with `args` in `dir`; panics with its output when it fails.
pub(super) fn run_7z(dir: &Path, args: &[&str]) -> String {
    let seven = seven_zip().expect("7-Zip");
    let out = Command::new(seven)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "7z {args:?}: {text}");
    text
}

/// A source tree: text that compresses, an "executable", a precompressed file, an empty file
/// and folder, a non-ASCII name.
pub(super) fn sample_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("sub/empty")).unwrap();
    let text: String = (0..4000)
        .map(|i| format!("line {i} of some text\n"))
        .collect();
    fs::write(src.join("a.txt"), &text).unwrap();
    fs::write(src.join("sub/b.log"), text.repeat(3)).unwrap();
    let exe: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(src.join("tool.exe"), &exe).unwrap();
    let noise: Vec<u8> = (0..100_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(src.join("photo.jpg"), &noise).unwrap();
    fs::write(src.join("zero.bin"), b"").unwrap();
    fs::write(src.join("Проект 日本.md"), b"# hi").unwrap();
    src
}

/// Every file under `root`, by `/`-separated relative path, with its bytes.
pub(super) fn tree_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn opts_7z(preset: Preset, solid: bool, password: Option<&str>, names: bool) -> CreateOptions {
    CreateOptions {
        preset,
        solid,
        encryption: password.map_or(Encryption::None, |p| Encryption::Aes256(Password::new(p))),
        encrypt_names: names,
        ..Default::default()
    }
}

fn extract_all(archive: &Path, dest: &Path, password: Option<&str>) -> ExtractReport {
    fs::create_dir_all(dest).unwrap();
    let (c, p) = ctx_parts();
    let opts = ExtractOptions {
        password: password.map(Password::new),
        ..Default::default()
    };
    extract(archive, dest, &opts, None, &JobCtx::new(&c, &p)).unwrap()
}

#[test]
fn round_trip_matrix() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let want = tree_files(&src);
    let mut n = 0;
    for preset in [Preset::Fast, Preset::Balanced, Preset::Max, Preset::Ultra] {
        for solid in [true, false] {
            for (password, names) in [(None, false), (Some("pässwörd"), false), (Some("pw"), true)]
            {
                n += 1;
                let out = d.path().join(format!("t{n}.7z"));
                let (c, p) = ctx_parts();
                let rep = create(
                    std::slice::from_ref(&src),
                    &out,
                    &opts_7z(preset, solid, password, names),
                    &JobCtx::new(&c, &p),
                )
                .unwrap();
                assert_eq!(rep.files, 6, "{preset:?}");
                assert!(rep.bytes_out > 0 && rep.bytes_out < rep.bytes_in);
                let listing =
                    list_with_password(&out, password.map(Password::new).as_ref()).unwrap();
                assert_eq!(listing.format, ArchiveFormat::SevenZip);
                assert_eq!(listing.any_encrypted(), password.is_some());
                if names {
                    assert!(matches!(list(&out), Err(CoreError::PasswordRequired(_))));
                }
                let x = d.path().join(format!("x{n}"));
                let r = extract_all(&out, &x, password);
                assert!(r.failed.is_empty() && r.skipped.is_empty(), "{r:?}");
                assert_eq!(tree_files(&x.join("src")), want, "{preset:?} solid={solid}");
                assert!(x.join("src/sub/empty").is_dir());
                if let Some(seven) = seven_zip() {
                    let pw = format!("-p{}", password.unwrap_or(""));
                    let out_s = out.to_string_lossy().into_owned();
                    let o = Command::new(seven)
                        .args(["t", &out_s, &pw])
                        .output()
                        .unwrap();
                    assert!(o.status.success(), "7-Zip test of {preset:?} solid={solid}");
                }
            }
        }
    }
}

#[test]
fn solid_entries_report_block_offsets_and_shares() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let out = d.path().join("s.7z");
    let (c, p) = ctx_parts();
    create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Balanced, true, None, false),
        &JobCtx::new(&c, &p),
    )
    .unwrap();
    let l = list(&out).unwrap();
    let get = |name: &str| l.entries.iter().find(|e| e.name == name).unwrap();
    let a = get("src/a.txt");
    let b = get("src/sub/b.log");
    assert!(a.solid && b.solid, "text files share a block");
    assert!(a.block_offset == 0 || b.block_offset == 0);
    assert!(a.block_offset + b.block_offset > 0);
    // The precompressed file is stored in a block of its own.
    let jpg = get("src/photo.jpg");
    assert!(!jpg.solid && jpg.block_offset == 0);
    assert_eq!(jpg.method, "Copy");
    assert_eq!(get("src/tool.exe").method, "LZMA2 BCJ");
    assert!(a.compressed_size > 0 && a.compressed_size < a.size);
}

#[test]
fn wrong_and_missing_passwords_are_told_apart_from_damage() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    for names in [false, true] {
        let out = d.path().join(format!("p{names}.7z"));
        let (c, p) = ctx_parts();
        let ctx = JobCtx::new(&c, &p);
        create(
            std::slice::from_ref(&src),
            &out,
            &opts_7z(Preset::Fast, true, Some("right"), names),
            &ctx,
        )
        .unwrap();
        let x = d.path().join(format!("w{names}"));
        fs::create_dir_all(&x).unwrap();
        for (pw, expect_wrong) in [(None, false), (Some("wrong"), true), (Some("Right"), true)] {
            let opts = ExtractOptions {
                password: pw.map(Password::new),
                ..Default::default()
            };
            let e = extract(&out, &x, &opts, None, &ctx).unwrap_err();
            if expect_wrong {
                assert!(
                    matches!(e, CoreError::WrongPassword(_)),
                    "{pw:?} {names}: {e}"
                );
            } else {
                assert!(matches!(e, CoreError::PasswordRequired(_)), "{names}: {e}");
            }
            assert_eq!(fs::read_dir(&x).unwrap().count(), 0, "nothing written");
        }
        let lp = test_archive(
            &out,
            Some(&Password::new("right")),
            &ExtractLimits::default(),
            &ctx,
        )
        .unwrap();
        assert!(lp.is_ok(), "{lp:?}");
    }
    // Damage in a block of an unencrypted archive is damage, per entry.
    let out = d.path().join("plain.7z");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Fast, true, None, false),
        &ctx,
    )
    .unwrap();
    let mut bytes = fs::read(&out).unwrap();
    for b in &mut bytes[200..260] {
        *b ^= 0x5A;
    }
    fs::write(&out, &bytes).unwrap();
    let t = test_archive(&out, None, &ExtractLimits::default(), &ctx).unwrap();
    assert!(!t.is_ok());
    assert!(
        t.failures()
            .all(|f| !f.error.as_deref().unwrap().contains("password")),
        "{t:?}"
    );
}

#[test]
fn reads_archives_made_by_7zip() {
    let Some(_) = seven_zip() else { return };
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let want = tree_files(&src);
    let cases: &[(&str, &[&str])] = &[
        ("lzma2.7z", &["-m0=lzma2", "-mx=5"]),
        ("lzma.7z", &["-m0=lzma", "-ms=off"]),
        ("ppmd.7z", &["-m0=ppmd"]),
        ("bzip2.7z", &["-m0=bzip2"]),
        ("deflate.7z", &["-m0=deflate"]),
        ("copy.7z", &["-m0=copy"]),
        (
            "bcj2.7z",
            &[
                "-m0=bcj2", "-m1=lzma", "-m2=lzma", "-m3=lzma", "-mb0:1", "-mb0s1:2", "-mb0s2:3",
            ],
        ),
        ("arm64.7z", &["-m0=arm64", "-m1=lzma2"]),
        ("delta.7z", &["-m0=delta:4", "-m1=lzma2"]),
        ("aes.7z", &["-m0=lzma2", "-psecret"]),
        ("aes-hdr.7z", &["-m0=lzma2", "-psecret", "-mhe=on"]),
        ("solid-small.7z", &["-ms=1f"]),
    ];
    for (name, args) in cases {
        let out = d.path().join(name);
        let mut a = vec!["a", out.to_str().unwrap()];
        a.extend_from_slice(args);
        a.push("src");
        run_7z(d.path(), &a);
        let pw = args.iter().any(|a| a.starts_with("-p")).then_some("secret");
        let x = d.path().join(format!("x-{name}"));
        let r = extract_all(&out, &x, pw);
        assert!(r.failed.is_empty() && r.skipped.is_empty(), "{name}: {r:?}");
        assert_eq!(tree_files(&x.join("src")), want, "{name}");
    }
}

#[test]
fn selected_entries_of_a_solid_block_decode_it_once() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("many");
    fs::create_dir_all(&src).unwrap();
    for i in 0..40 {
        fs::write(
            src.join(format!("f{i:02}.txt")),
            format!("{i} ").repeat(2000),
        )
        .unwrap();
    }
    let out = d.path().join("m.7z");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Fast, true, None, false),
        &ctx,
    )
    .unwrap();
    let l = list(&out).unwrap();
    assert!(l.entries.iter().filter(|e| !e.is_dir).all(|e| e.solid));
    // Visit three files: one pass, in order, each with its own data.
    let mut seen = Vec::new();
    SEVEN_ZIP_FOR_TESTS
        .read_entries(
            &out,
            &|e| ["many/f05.txt", "many/f20.txt", "many/f39.txt"].contains(&e.name.as_str()),
            None,
            Some(&ctx),
            &mut |e, r| {
                let mut s = String::new();
                r.unwrap().read_to_string(&mut s).unwrap();
                seen.push((e.name.clone(), s.len()));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));
    let x = d.path().join("x");
    fs::create_dir_all(&x).unwrap();
    let sel = Selection::new("many/", vec!["many/f20.txt".into(), "many/f21.txt".into()]);
    let r = extract(&out, &x, &ExtractOptions::default(), Some(&sel), &ctx).unwrap();
    assert_eq!(r.extracted.len(), 2, "{r:?}");
    assert_eq!(
        fs::read_to_string(x.join("f21.txt")).unwrap(),
        "21 ".repeat(2000)
    );
}

static SEVEN_ZIP_FOR_TESTS: &dyn ArchiveBackend = &sevenz_backend::SEVEN_ZIP;

#[test]
fn cancelled_create_and_extract_leave_nothing() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let out = d.path().join("c.7z");
    let c = CancelToken::new();
    let p = Progress::default();
    c.cancel();
    let e = create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Fast, true, None, false),
        &JobCtx::new(&c, &p),
    )
    .unwrap_err();
    assert!(matches!(e, CoreError::Cancelled), "{e}");
    let leftovers: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "src")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn edit_keeps_untouched_blocks_and_recompresses_touched_ones() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let extra = d.path().join("extra.txt");
    fs::write(&extra, b"added later").unwrap();
    for password in [None, Some("pw")] {
        let out = d.path().join(format!("e{}.7z", password.is_some()));
        let (c, p) = ctx_parts();
        let ctx = JobCtx::new(&c, &p);
        let opts = opts_7z(Preset::Fast, true, password, true);
        create(std::slice::from_ref(&src), &out, &opts, &ctx).unwrap();
        let edit = ArchiveEdit {
            add: vec![(extra.clone(), "src/sub/".into())],
            delete: vec!["src/a.txt".into()],
        };
        let rep = update(&out, &edit, &opts, &ctx).unwrap();
        assert_eq!(rep.deleted, 1);
        let l = list_with_password(&out, password.map(Password::new).as_ref()).unwrap();
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"src/a.txt"), "{names:?}");
        assert!(names.contains(&"src/sub/extra.txt"), "{names:?}");
        assert!(names.contains(&"src/photo.jpg"));
        let x = d.path().join(format!("ex{}", password.is_some()));
        let r = extract_all(&out, &x, password);
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(
            fs::read(x.join("src/sub/extra.txt")).unwrap(),
            b"added later"
        );
        assert_eq!(
            fs::read(x.join("src/sub/b.log")).unwrap(),
            fs::read(src.join("sub/b.log")).unwrap()
        );
        if let Some(seven) = seven_zip() {
            let pw = format!("-p{}", password.unwrap_or(""));
            let o = Command::new(seven)
                .args(["t", out.to_str().unwrap(), &pw])
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        }
    }
}

// ---------------------------------------------------------------------------
// Hand-built headers: bombs are refused before anything is allocated or decoded
// ---------------------------------------------------------------------------

/// 7z's variable-length number.
fn num(v: u64) -> Vec<u8> {
    if v < 0x80 {
        return vec![v as u8];
    }
    let mut out = vec![0xFF];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

/// A 7z file: signature header, `packed` data, then `header` as the next header.
fn raw_7z(packed: &[u8], header: &[u8]) -> Vec<u8> {
    let mut start = Vec::new();
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(header.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32fast::hash(header).to_le_bytes());
    let mut out = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
    out.extend_from_slice(&crc32fast::hash(&start).to_le_bytes());
    out.extend_from_slice(&start);
    out.extend_from_slice(packed);
    out.extend_from_slice(header);
    out
}

/// Streams info with one pack stream of `pack` bytes and one folder of one coder.
fn one_folder(pack: u64, id: &[u8], props: &[u8], unpack: u64) -> Vec<u8> {
    let mut h = vec![0x06];
    h.extend(num(0));
    h.extend(num(1));
    h.push(0x09);
    h.extend(num(pack));
    h.push(0x00);
    h.push(0x07);
    h.push(0x0B);
    h.extend(num(1));
    h.push(0); // not external
    h.extend(num(1)); // coders
    h.push(id.len() as u8 | if props.is_empty() { 0 } else { 0x20 });
    h.extend_from_slice(id);
    if !props.is_empty() {
        h.extend(num(props.len() as u64));
        h.extend_from_slice(props);
    }
    h.push(0x0C);
    h.extend(num(unpack));
    h.push(0x00);
    h
}

/// A plain header holding one file named `f.txt` whose data is a block with `id`/`props`.
fn file_header(pack: u64, id: &[u8], props: &[u8], unpack: u64) -> Vec<u8> {
    let mut h = vec![0x01, 0x04];
    h.extend(one_folder(pack, id, props, unpack));
    h.extend([0x08, 0x00]); // sub-streams: one per folder
    h.push(0x00); // end of streams info
    h.push(0x05);
    h.extend(num(1));
    let name: Vec<u8> = "f.txt\0"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    h.push(0x11);
    h.extend(num(name.len() as u64 + 1));
    h.push(0);
    h.extend(name);
    h.push(0x00); // end of files info
    h.push(0x00); // end of header
    h
}

fn write_raw(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn assert_refused(path: &Path, needle: &str) {
    let e = list(path).unwrap_err();
    assert!(e.to_string().contains(needle), "{}: {e}", path.display());
}

#[test]
fn hand_built_plain_header_lists_and_extracts() {
    // The builder makes archives the reader accepts: the bomb tests below differ in one value.
    let d = tempfile::tempdir().unwrap();
    let data = b"hello 7z";
    let p = write_raw(
        d.path(),
        "ok.7z",
        &raw_7z(
            data,
            &file_header(data.len() as u64, &[0x00], &[], data.len() as u64),
        ),
    );
    let l = list(&p).unwrap();
    assert_eq!(l.entries.len(), 1);
    assert_eq!(l.entries[0].name, "f.txt");
    let x = d.path().join("x");
    let r = extract_all(&p, &x, None);
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(x.join("f.txt")).unwrap(), data);
}

#[test]
fn header_bombs_are_refused_before_allocating() {
    let d = tempfile::tempdir().unwrap();
    // A billion entries declared in a few bytes.
    let mut h = vec![0x01, 0x05];
    h.extend(num(1_000_000_000));
    h.extend([0x00, 0x00]);
    assert_refused(
        &write_raw(d.path(), "files.7z", &raw_7z(&[], &h)),
        "entries",
    );
    // 100 coders in one folder.
    let mut h = vec![0x01, 0x04, 0x07, 0x0B];
    h.extend(num(1));
    h.push(0);
    h.extend(num(100));
    h.extend([0x00; 64]);
    assert_refused(
        &write_raw(d.path(), "coders.7z", &raw_7z(&[], &h)),
        "coders",
    );
    // A billion folders.
    let mut h = vec![0x01, 0x04, 0x07, 0x0B];
    h.extend(num(1_000_000_000));
    h.extend([0x00; 16]);
    assert_refused(
        &write_raw(d.path(), "folders.7z", &raw_7z(&[], &h)),
        "folders",
    );
    // A next header bigger than the file, and one bigger than allowed.
    let mut bytes = raw_7z(&[], &[0x01, 0x00]);
    bytes[20..28].copy_from_slice(&(1u64 << 40).to_le_bytes());
    let start_crc = crc32fast::hash(&bytes[12..32]);
    bytes[8..12].copy_from_slice(&start_crc.to_le_bytes());
    assert_refused(
        &write_raw(d.path(), "past.7z", &bytes),
        "larger than allowed",
    );
    let mut bytes = raw_7z(&[], &[0x01, 0x00]);
    bytes[20..28].copy_from_slice(&(1u64 << 20).to_le_bytes());
    let start_crc = crc32fast::hash(&bytes[12..32]);
    bytes[8..12].copy_from_slice(&start_crc.to_le_bytes());
    assert_refused(&write_raw(d.path(), "past2.7z", &bytes), "past the end");
}

#[test]
fn encoded_header_bombs_are_refused_before_decoding() {
    let d = tempfile::tempdir().unwrap();
    let packed = [0u8; 32];
    // LZMA with a 4 GiB dictionary.
    let mut h = vec![0x17];
    h.extend(one_folder(
        32,
        &[0x03, 0x01, 0x01],
        &[0x5D, 0xFF, 0xFF, 0xFF, 0xFF],
        100,
    ));
    h.push(0x00);
    assert_refused(
        &write_raw(d.path(), "dict.7z", &raw_7z(&packed, &h)),
        "decoder memory",
    );
    // A header that would expand to a terabyte.
    let mut h = vec![0x17];
    h.extend(one_folder(
        32,
        &[0x03, 0x01, 0x01],
        &[0x5D, 0, 0, 1, 0],
        1 << 40,
    ));
    h.push(0x00);
    assert_refused(
        &write_raw(d.path(), "huge.7z", &raw_7z(&packed, &h)),
        "expands to more than",
    );
    // An AES-encrypted header without a password is a password question, not damage.
    let mut h = vec![0x17];
    let mut folder = one_folder(32, &[0x06, 0xF1, 0x07, 0x01], &[0x13, 0x00], 32);
    folder.pop();
    folder.extend([0x0A, 0x01]); // the decoded header's CRC
    folder.extend(0xDEAD_BEEFu32.to_le_bytes());
    folder.push(0x00);
    h.extend(folder);
    h.push(0x00);
    let p = write_raw(d.path(), "aes.7z", &raw_7z(&packed, &h));
    assert!(matches!(list(&p), Err(CoreError::PasswordRequired(_))));
    assert!(matches!(
        list_with_password(&p, Some(&Password::new("x"))),
        Err(CoreError::WrongPassword(_))
    ));
}

#[test]
fn data_block_with_a_huge_dictionary_is_refused_before_writing() {
    let d = tempfile::tempdir().unwrap();
    let packed = [0u8; 16];
    // LZMA2 property 40: a 4 GiB dictionary.
    let p = write_raw(
        d.path(),
        "lzma2.7z",
        &raw_7z(&packed, &file_header(16, &[0x21], &[40], 1000)),
    );
    let l = list(&p).unwrap();
    assert_eq!(l.entries[0].method, "LZMA2");
    let x = d.path().join("x");
    fs::create_dir_all(&x).unwrap();
    let (c, pr) = ctx_parts();
    let e = extract(
        &p,
        &x,
        &ExtractOptions::default(),
        None,
        &JobCtx::new(&c, &pr),
    )
    .unwrap_err();
    assert!(e.to_string().contains("decoder memory"), "{e}");
    assert_eq!(fs::read_dir(&x).unwrap().count(), 0);
    // PPMd asking for 4 GiB of model memory, the same.
    let p = write_raw(
        d.path(),
        "ppmd.7z",
        &raw_7z(
            &packed,
            &file_header(16, &[0x03, 0x04, 0x01], &[6, 0xFF, 0xFF, 0xFF, 0xFF], 1000),
        ),
    );
    let e = extract(
        &p,
        &x,
        &ExtractOptions::default(),
        None,
        &JobCtx::new(&c, &pr),
    )
    .unwrap_err();
    assert!(e.to_string().contains("decoder memory"), "{e}");
}

#[test]
fn incomplete_and_truncated_archives_are_damage() {
    let d = tempfile::tempdir().unwrap();
    let mut bytes = raw_7z(b"data", &[0x01, 0x00]);
    bytes[8..32].fill(0);
    assert_refused(&write_raw(d.path(), "incomplete.7z", &bytes), "incomplete");
    let src = sample_tree(d.path());
    let out = d.path().join("t.7z");
    let (c, p) = ctx_parts();
    create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Fast, true, None, false),
        &JobCtx::new(&c, &p),
    )
    .unwrap();
    let full = fs::read(&out).unwrap();
    let cut = write_raw(d.path(), "cut.7z", &full[..full.len() / 2]);
    assert!(matches!(list(&cut), Err(CoreError::Invalid(_))));
}

// ---------------------------------------------------------------------------
// Review round 1: hostile pack sizes, unproven passwords
// ---------------------------------------------------------------------------

/// A plain header with only pack info: `sizes` packed streams from position `pos`.
fn packs_header(pos: u64, sizes: &[u64]) -> Vec<u8> {
    let mut h = vec![0x01, 0x04, 0x06];
    h.extend(num(pos));
    h.extend(num(sizes.len() as u64));
    h.push(0x09);
    for &s in sizes {
        h.extend(num(s));
    }
    h.extend([0x00, 0x00, 0x00]);
    h
}

#[test]
fn pack_sizes_past_u64_or_the_file_are_refused_without_panicking() {
    let d = tempfile::tempdir().unwrap();
    let data = [7u8; 16];
    // Sizes adding up past u64::MAX.
    let p = write_raw(
        d.path(),
        "wrap.7z",
        &raw_7z(&data, &packs_header(0, &[u64::MAX - 10, 100])),
    );
    assert_refused(&p, "past the end");
    // A position that overflows with the signature header.
    let p = write_raw(
        d.path(),
        "pos.7z",
        &raw_7z(&data, &packs_header(u64::MAX - 8, &[1])),
    );
    assert_refused(&p, "past the end");
    // A packed stream running past the end of the file.
    let p = write_raw(
        d.path(),
        "eof.7z",
        &raw_7z(&data, &packs_header(0, &[1000])),
    );
    assert_refused(&p, "past the end");
    // The files of a folder adding up to more than the folder.
    let mut h = vec![0x01, 0x04];
    h.extend(one_folder(16, &[0x00], &[], 16));
    h.extend([0x08, 0x0D, 0x02, 0x09]);
    h.extend(num(u64::MAX));
    h.extend([0x00, 0x00, 0x00]);
    let p = write_raw(d.path(), "subs.7z", &raw_7z(&data, &h));
    assert_refused(&p, "larger than their folder");
    // The archive reads fine when the same layout is consistent.
    let p = write_raw(
        d.path(),
        "fine.7z",
        &raw_7z(&data, &file_header(16, &[0x00], &[], 16)),
    );
    assert!(list(&p).is_ok());
}

#[test]
fn an_archive_without_crcs_leaves_a_password_unverified() {
    let d = tempfile::tempdir().unwrap();
    // One AES coder (one key derivation round, no salt or IV) and no CRC anywhere.
    let p = write_raw(
        d.path(),
        "nocrc.7z",
        &raw_7z(
            &[0x42; 16],
            &file_header(16, &[0x06, 0xF1, 0x07, 0x01], &[0x01], 16),
        ),
    );
    let l = list(&p).unwrap();
    let files: Vec<&ArchiveEntry> = l.entries.iter().collect();
    assert!(files[0].encrypted);
    assert_eq!(
        sevenz_backend::verify_password(&p, &files, Some(&Password::new("any"))).unwrap(),
        sevenz_backend::PasswordCheck::Unverified
    );
}

#[test]
fn a_wrong_password_that_decodes_a_prefix_is_not_accepted() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("big");
    fs::create_dir_all(&src).unwrap();
    // Stored (Copy) data larger than the proof budget: any key decrypts its start.
    let size = sevenz_backend::PASSWORD_PROOF_BUDGET as usize + 1024 * 1024;
    let noise: Vec<u8> = (0..size as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(src.join("movie.mp4"), &noise).unwrap();
    let out = d.path().join("stored.7z");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    create(
        std::slice::from_ref(&src),
        &out,
        &opts_7z(Preset::Fast, true, Some("right"), false),
        &ctx,
    )
    .unwrap();
    let l = list(&out).unwrap();
    let files: Vec<&ArchiveEntry> = l.entries.iter().filter(|e| !e.is_dir).collect();
    for pw in ["right", "wrong"] {
        assert_eq!(
            sevenz_backend::verify_password(&out, &files, Some(&Password::new(pw))).unwrap(),
            sevenz_backend::PasswordCheck::Unverified,
            "{pw}: a decodable prefix proves nothing"
        );
    }
    // Extraction goes ahead, but the file fails its CRC and is never published.
    let x = d.path().join("x");
    fs::create_dir_all(&x).unwrap();
    let opts = ExtractOptions {
        password: Some(Password::new("wrong")),
        ..Default::default()
    };
    let r = extract(&out, &x, &opts, None, &ctx).unwrap();
    assert!(r.extracted.is_empty(), "{r:?}");
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(
        r.failed[0]
            .reason
            .contains("damaged data or wrong password"),
        "{r:?}"
    );
    assert!(!x.join("big/movie.mp4").exists());
    // The right password extracts it.
    let opts = ExtractOptions {
        password: Some(Password::new("right")),
        ..Default::default()
    };
    let r = extract(&out, &x, &opts, None, &ctx).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(x.join("big/movie.mp4")).unwrap(), noise);
    // Files are never added with a password nothing confirmed, right or wrong.
    let extra = d.path().join("extra.txt");
    fs::write(&extra, b"new").unwrap();
    for pw in ["right", "wrong"] {
        let edit = ArchiveEdit {
            add: vec![(extra.clone(), String::new())],
            delete: vec![],
        };
        let e = update(
            &out,
            &edit,
            &opts_7z(Preset::Fast, true, Some(pw), false),
            &ctx,
        )
        .unwrap_err();
        assert!(e.to_string().contains("can't be confirmed"), "{pw}: {e}");
    }
    assert_eq!(list(&out).unwrap().entries.len(), l.entries.len());
}

// ---------------------------------------------------------------------------
// Review round 2: the format comes from the content, not the extension
// ---------------------------------------------------------------------------

#[test]
fn renamed_archives_are_read_and_edited_as_what_they_are() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let extra = d.path().join("extra.txt");
    fs::write(&extra, b"added later").unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    // (made as, renamed to, format it must stay)
    let cases = [
        (
            "made.7z",
            "looks-like.zip",
            ArchiveFormat::SevenZip,
            &b"7z\xBC\xAF\x27\x1C"[..],
        ),
        (
            "made.zip",
            "looks-like.7z",
            ArchiveFormat::Zip,
            &b"PK\x03\x04"[..],
        ),
    ];
    for (made, renamed, format, magic) in cases {
        let made = d.path().join(made);
        create(
            std::slice::from_ref(&src),
            &made,
            &CreateOptions::default(),
            &ctx,
        )
        .unwrap();
        let renamed = d.path().join(renamed);
        fs::rename(&made, &renamed).unwrap();
        let l = list(&renamed).unwrap();
        assert_eq!(l.format, format, "{}", renamed.display());
        assert!(index_cached(&renamed).is_ok());
        let x = d.path().join(format!("x-{format:?}"));
        let r = extract_all(&renamed, &x, None);
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(tree_files(&x.join("src")), tree_files(&src));
        let t = test_archive(&renamed, None, &ExtractLimits::default(), &ctx).unwrap();
        assert!(t.is_ok(), "{t:?}");
        // An edit keeps the real format under the same name.
        let edit = ArchiveEdit {
            add: vec![(extra.clone(), String::new())],
            delete: vec!["src/a.txt".into()],
        };
        update(&renamed, &edit, &CreateOptions::default(), &ctx).unwrap();
        assert!(fs::read(&renamed).unwrap().starts_with(magic), "{format:?}");
        let l = list(&renamed).unwrap();
        assert_eq!(l.format, format);
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"extra.txt") && !names.contains(&"src/a.txt"),
            "{names:?}"
        );
    }
}

#[test]
fn a_zip_behind_a_stub_is_found_by_its_end_record() {
    let d = tempfile::tempdir().unwrap();
    let src = sample_tree(d.path());
    let zip = d.path().join("a.zip");
    let (c, p) = ctx_parts();
    create(
        std::slice::from_ref(&src),
        &zip,
        &CreateOptions::default(),
        &JobCtx::new(&c, &p),
    )
    .unwrap();
    // A self-extractor's stub in front, under a name that says nothing.
    let mut bytes = b"MZ fake self-extractor stub".to_vec();
    bytes.extend(fs::read(&zip).unwrap());
    let sfx = d.path().join("setup.bin");
    fs::write(&sfx, &bytes).unwrap();
    assert_eq!(backend_for_file(&sfx).unwrap().format(), ArchiveFormat::Zip);
}
