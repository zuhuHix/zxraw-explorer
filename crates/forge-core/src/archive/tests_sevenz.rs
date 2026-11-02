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
    let out = Command::new(seven).current_dir(dir).args(args).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "7z {args:?}: {text}");
    text
}

/// A source tree: text that compresses, an "executable", a precompressed file, an empty file
/// and folder, a non-ASCII name.
pub(super) fn sample_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("sub/empty")).unwrap();
    let text: String = (0..4000).map(|i| format!("line {i} of some text\n")).collect();
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
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
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
                let listing = list_with_password(&out, password.map(Password::new).as_ref())
                    .unwrap();
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
                    let o = Command::new(seven).args(["t", &out_s, &pw]).output().unwrap();
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
                assert!(matches!(e, CoreError::WrongPassword(_)), "{pw:?} {names}: {e}");
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
        t.failures().all(|f| !f.error.as_deref().unwrap().contains("password")),
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
        ("bcj2.7z", &["-m0=bcj2", "-m1=lzma", "-m2=lzma", "-m3=lzma", "-mb0:1", "-mb0s1:2", "-mb0s2:3"]),
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
        fs::write(src.join(format!("f{i:02}.txt")), format!("{i} ").repeat(2000)).unwrap();
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
    assert_eq!(fs::read_to_string(x.join("f21.txt")).unwrap(), "21 ".repeat(2000));
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
        assert_eq!(fs::read(x.join("src/sub/extra.txt")).unwrap(), b"added later");
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
