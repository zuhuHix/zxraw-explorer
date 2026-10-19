//! End-to-end scenarios: browser grouping.

#[allow(unused_imports)]
use crate::*;

use image::RgbImage;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

const BURST_HASH: u64 = 0x1234_5678_9abc_def0;
const DUPLICATE_HASH: u64 = 0x0f0f_f0f0_aaaa_5555;

fn jpeg_for_hash(hash: u64, seed: u32) -> Vec<u8> {
    let image = pixels_for_hash(hash, seed);
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 94)
        .encode_image(&image)
        .unwrap();
    bytes
}

fn png_for_hash(hash: u64, seed: u32) -> Vec<u8> {
    let image = pixels_for_hash(hash, seed);
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn pixels_for_hash(hash: u64, seed: u32) -> RgbImage {
    const WIDTH: u32 = 144;
    const HEIGHT: u32 = 128;
    let mut state = seed.max(1);
    RgbImage::from_fn(WIDTH, HEIGHT, |x, y| {
        let cell_x = (x * 9 / WIDTH).min(8) as usize;
        let cell_y = (y * 8 / HEIGHT).min(7) as usize;
        let mut level = 112i16;
        for bit in cell_y * 8..cell_y * 8 + cell_x {
            level += if (hash >> bit) & 1 == 1 { -12 } else { 12 };
        }
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let noise = ((state >> 16) % 5) as i16 - 2;
        let value = (level + noise).clamp(0, 255) as u8;
        image::Rgb([value, value, value])
    })
}

fn write_jpeg(dir: &Path, name: &str, hash: u64, seed: u32, mtime: u64) {
    let path = dir.join(name);
    write_file(&path, &jpeg_for_hash(hash, seed));
    set_mtime(&path, mtime);
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = File::options().write(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
    )
    .unwrap();
}

fn fixture() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let work = root.join("work");
    let profile = root.join("profile");
    (temp, work, profile, root)
}

#[test]
fn bursts_and_far_apart_visual_duplicates_group_select_copy_and_screenshot() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("source");
    let destination = root.join("destination");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&destination).unwrap();
    let base = 1_700_000_000;

    for shot in 0..5 {
        write_jpeg(
            &source,
            &format!("burst-{shot}.jpg"),
            BURST_HASH,
            shot + 1,
            base + shot as u64,
        );
    }
    write_jpeg(&source, "unrelated-a.jpg", 0, 31, base + 3_600);
    write_jpeg(&source, "duplicate-a.jpg", DUPLICATE_HASH, 41, base + 4_800);
    write_jpeg(&source, "unrelated-b.jpg", u64::MAX, 51, base + 5_400);
    write_jpeg(
        &source,
        "unrelated-c.jpg",
        0xaaaa_aaaa_aaaa_aaaa,
        61,
        base + 7_200,
    );
    write_jpeg(&source, "duplicate-b.jpg", DUPLICATE_HASH, 71, base + 8_500);

    let screenshot_dir = shots_dir().join("browser");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let shot_stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let shot_prefix = format!("similar-{}-{shot_stamp}", std::process::id());
    let collapsed = screenshot_dir.join(format!("{shot_prefix}-collapsed.png"));
    let expanded = screenshot_dir.join(format!("{shot_prefix}-expanded.png"));
    let copied: Vec<String> = (0..5)
        .map(|shot| shown(&destination.join(format!("burst-{shot}.jpg"))))
        .collect();
    let mut script = format!(
        "limit 180\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 120\n\
         ui browser assert-groups 2\n\
         ui browser assert-members burst-0.jpg | burst-0.jpg, burst-1.jpg, burst-2.jpg, burst-3.jpg, burst-4.jpg\n\
         ui browser assert-members duplicate-a.jpg | duplicate-a.jpg, duplicate-b.jpg\n\
         ui viewer thumbs-idle 60\nui browser collapse burst-0.jpg\nwait-frames 3\n\
         ui browser assert-collapsed burst-0.jpg\nscreenshot {}\n\
         ui browser expand burst-0.jpg\nwait-frames 3\nui browser assert-expanded burst-0.jpg\nscreenshot {}\n\
         ui browser collapse burst-0.jpg\nwait-frames 2\n\
         ui browser select-group burst-0.jpg\ncmd ToggleDualPane\nfocus right\nnavigate {}\nfocus left\ncmd CopyToOtherPane\n",
        shown(&source),
        shown(&collapsed),
        shown(&expanded),
        shown(&destination),
    );
    for path in &copied {
        script.push_str(&format!("wait-exists {path} | 60\n"));
    }
    for shot in 0..5 {
        script.push_str(&format!(
            "assert-same {} | {}\n",
            shown(&source.join(format!("burst-{shot}.jpg"))),
            shown(&destination.join(format!("burst-{shot}.jpg")))
        ));
    }
    script.push_str("assert-no-error-toast\nexit\n");

    let outcome = run_app_with(
        &work,
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(180)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        collapsed.is_file(),
        "missing collapsed screenshot: {}",
        collapsed.display()
    );
    assert!(
        expanded.is_file(),
        "missing expanded screenshot: {}",
        expanded.display()
    );
    println!(
        "browser grouping screenshots:\n collapsed: {}\n expanded: {}",
        collapsed.display(),
        expanded.display()
    );
}

#[test]
fn corrupt_jpeg_in_burst_does_not_break_grouping_or_crash() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("corrupt-source");
    fs::create_dir_all(&source).unwrap();
    let base = 1_700_100_000;
    write_jpeg(&source, "valid-before.jpg", BURST_HASH, 1, base);
    let corrupt = source.join("corrupt.jpg");
    write_file(&corrupt, &noise(1_024, 0xface));
    set_mtime(&corrupt, base + 1);
    write_jpeg(&source, "valid-after.jpg", BURST_HASH, 2, base + 2);

    let script = format!(
        "limit 90\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 60\n\
         ui browser assert-groups 1\n\
         ui browser assert-members valid-before.jpg | valid-before.jpg, corrupt.jpg, valid-after.jpg\n\
         assert-no-error-toast\nexit\n",
        shown(&source)
    );
    let outcome = run_app_with(&work, &profile, &script, &RunOpts::default());
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn two_thousand_small_images_record_grouping_frame_times() {
    // Frame timings: runs alone, with no other app window running.
    let _alone = exclusive();
    let (_temp, work, profile, root) = fixture();
    let source = root.join("many-images");
    fs::create_dir_all(&source).unwrap();
    let base = 1_701_000_000;
    let mut state = 0x1234_5678u32;
    let variants: Vec<Vec<u8>> = (0..64)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            png_for_hash((state as u64) << 32 | state.rotate_left(13) as u64, state)
        })
        .collect();
    for index in 0..2_000 {
        let path = source.join(format!("image-{index:04}.png"));
        write_file(&path, &variants[index % variants.len()]);
        set_mtime(&path, base + index as u64 * 60);
    }

    let report = root.join("grouping-perf.json");
    let script = format!(
        "limit 360\nnavigate {}\ncmd ViewGrid\nui perf begin\nui browser toggle\n\
         ui browser wait 300\nui browser assert-groups 0\nui perf end {}\nexit\n",
        shown(&source),
        shown(&report)
    );
    let outcome = run_app_with(
        &work,
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(360)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let perf = fs::read_to_string(&report).unwrap();
    println!("2,000-image grouping frame performance:\n{perf}");
}

/// Marks a file hidden the way Explorer does (`attrib +h`), so the listing leaves it out.
fn hide(path: &Path) {
    let status = std::process::Command::new("attrib")
        .arg("+h")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "attrib +h failed for {}", path.display());
}

#[test]
fn hidden_burst_member_is_not_selected_or_copied_with_its_stack() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("hidden-source");
    let destination = root.join("hidden-destination");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&destination).unwrap();
    let base = 1_702_000_000;
    for shot in 0..3u32 {
        write_jpeg(
            &source,
            &format!("shot-{}.jpg", shot + 1),
            BURST_HASH,
            shot + 1,
            base + shot as u64,
        );
    }
    hide(&source.join("shot-2.jpg"));

    let screenshot_dir = shots_dir().join("browser");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let stack_shot = screenshot_dir.join("similar-hidden-stack.png");
    let copied = destination.join("shot-1.jpg");
    let copied_last = destination.join("shot-3.jpg");
    let hidden_copy = destination.join("shot-2.jpg");
    let script = format!(
        "limit 120\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 60\n\
         ui browser assert-groups 1\n\
         ui browser assert-members shot-1.jpg | shot-1.jpg, shot-3.jpg\n\
         wait-frames 3\nscreenshot {}\n\
         ui browser select-group shot-1.jpg\n\
         cmd ToggleDualPane\nfocus right\nnavigate {}\nfocus left\ncmd CopyToOtherPane\n\
         wait-exists {} | 60\nwait-exists {} | 60\nwait-jobs\n\
         assert-missing {}\n\
         assert-same {} | {}\n\
         assert-same {} | {}\n\
         assert-no-error-toast\nexit\n",
        shown(&source),
        shown(&stack_shot),
        shown(&destination),
        shown(&copied),
        shown(&copied_last),
        shown(&hidden_copy),
        shown(&source.join("shot-1.jpg")),
        shown(&copied),
        shown(&source.join("shot-3.jpg")),
        shown(&copied_last),
    );
    let outcome = run_app_with(&work, &profile, &script, &RunOpts::default());
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        stack_shot.is_file(),
        "missing stack screenshot: {}",
        stack_shot.display()
    );
    println!("hidden-member stack screenshot: {}", stack_shot.display());
}

fn list_fixture(names: &[&str]) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    for name in names {
        write_file(&work.join(name), b"x");
    }
    let profile = temp.path().join("profile");
    (temp, work, profile)
}

#[test]
fn typing_filters_the_list_when_the_setting_says_so() {
    let (temp, work, profile) = list_fixture(&["beach 01.txt", "beach 02.txt", "city.txt"]);
    write_file(
        &profile.join("config").join("settings.json"),
        br#"{ "list_typing": "Filter" }"#,
    );
    let shot = shots_dir().join("type-to-filter.png");
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         # Nothing focused: the app's global key handler takes the text.\n\
         text cit\nui browser assert-filter cit\nui browser assert-visible city.txt\n\
         key Escape\nui browser assert-filter\n\
         # A focused row: the list view's own key handler takes it.\n\
         click-entry city.txt\ntext beach 0\nwait-frames 2\nscreenshot {shot}\n\
         ui browser assert-filter beach 0\n\
         ui browser assert-visible beach 01.txt, beach 02.txt\n\
         key Backspace\nkey Backspace\nui browser assert-filter beach\nassert-path {work}\n\
         key Escape\nui browser assert-visible beach 01.txt, beach 02.txt, city.txt\n\
         # With no filter left, Backspace goes up a folder as usual.\n\
         click-entry city.txt\nkey Backspace\nassert-path {root}\nexit\n",
        work = shown(&work),
        root = shown(temp.path()),
        shot = shown(&shot),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn typing_jumps_to_a_name_by_default() {
    let (temp, work, profile) = list_fixture(&["beach.txt", "city.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         text ci\nassert-selection city.txt\nui browser assert-filter\n\
         ui browser assert-visible beach.txt, city.txt\nexit\n",
        work = shown(&work),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn sticky_selection_toggles_items_with_plain_clicks() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt"]);
    let shot = shots_dir().join("sticky-selection.png");
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         key ctrl+shift+S\nui browser assert-sticky on\n\
         click-entry a.txt\nclick-entry b.txt\nclick-entry c.txt\nassert-selected 3\n\
         click-entry b.txt\nassert-selection a.txt, c.txt\nwait-frames 2\nscreenshot {shot}\n\
         # Esc ends sticky selection and keeps the picks; a plain click replaces them again.\n\
         key Escape\nui browser assert-sticky off\nassert-selection a.txt, c.txt\n\
         click-entry b.txt\nassert-selection b.txt\n\
         # The command toggles it too.\n\
         cmd ToggleStickySelection\nclick-entry a.txt\nassert-selection a.txt, b.txt\n\
         cmd ToggleStickySelection\nui browser assert-sticky off\n\
         # Esc in the empty filter box ends sticky selection too, keeping the picks.\n\
         cmd ToggleStickySelection\ncmd FocusFilter\nwait-frames 2\nkey Escape\n\
         ui browser assert-sticky off\nassert-selection a.txt, b.txt\nexit\n",
        work = shown(&work),
        shot = shown(&shot),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn folders_open_in_place_with_arrows_and_clicks() {
    let (temp, work, profile) = list_fixture(&[
        "Shoot/img_02.txt",
        "Shoot/img_01.txt",
        "Shoot/Selects/best.txt",
        "notes.txt",
    ]);
    let shot = shots_dir().join("folders-open-in-place.png");
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         # A click on the arrow opens the folder in place, lazily.\n\
         ui browser click-arrow Shoot\nwait-entry img_01.txt\nwait-frames 3\n\
         ui browser assert-visible Shoot, Selects, img_01.txt, img_02.txt, notes.txt\n\
         ui browser assert-depths Shoot=0, Selects=1, img_01.txt=1, notes.txt=0\n\
         # Right opens a folder under the cursor; Left goes to its parent, then closes it.\n\
         click-entry Selects\nkey ArrowRight\nwait-entry best.txt\nwait-frames 3\n\
         ui browser assert-depths best.txt=2\nscreenshot {shot}\n\
         click-entry best.txt\nkey ArrowLeft\nassert-selection Selects\n\
         key ArrowLeft\nui browser assert-collapsed-folder Selects\n\
         # Focus stays in the list: Left again goes from Selects to its folder.\n\
         key ArrowLeft\nassert-selection Shoot\n\
         # Multi-select across levels.\n\
         click-entry img_02.txt\nkey ctrl+A\nassert-selected 5\n\
         # The filter applies to nested items, and keeps the folders that lead to a match.\n\
         cmd FocusFilter\nwait-frames 2\ntext img_01\nwait-frames 2\n\
         ui browser assert-visible Shoot, img_01.txt\nkey Escape\nwait-frames 2\n\
         ui browser assert-visible Shoot, Selects, img_01.txt, img_02.txt, notes.txt\n\
         # Delete a nested file: the open folder follows the change.\n\
         click-entry img_02.txt\nkey Delete\nwait-jobs 20\nwait-no-entry img_02.txt\n\
         assert-missing {img02}\nassert-path {work}\n\
         # Double-clicking a nested folder opens it as the current folder.\n\
         # (Let the refresh that follows the delete finish: steps pause while a pane loads.)\n\
         wait-frames 60\ndouble-click-entry Selects\nwait-entry best.txt\nassert-path {selects}\n\
         navigate {work}\nwait-entry img_01.txt\nui browser assert-expanded-folder Shoot\n\
         # A change inside a nested open folder shows up: open folders are watched too.\n\
         ui browser click-arrow Selects\nwait-entry best.txt\n\
         ui tools external create {late}\nwait-entry late.txt | 15\n\
         ui browser assert-depths late.txt=2\nexit\n",
        late = shown(&work.join("Shoot").join("Selects").join("late.txt")),
        work = shown(&work),
        shot = shown(&shot),
        img02 = shown(&work.join("Shoot").join("img_02.txt")),
        selects = shown(&work.join("Shoot").join("Selects")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn delete_with_the_selection_filtered_out_touches_no_file() {
    let (temp, work, profile) = list_fixture(&["apple.txt", "berry.txt", "cherry.txt"]);
    write_file(
        &profile.join("config").join("settings.json"),
        br#"{ "list_typing": "Filter" }"#,
    );
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         # Select cherry, then type a filter that hides it (the cursor lands on berry).\n\
         click-entry cherry.txt\ntext berr\nui browser assert-visible berry.txt\n\
         key Delete\nwait-frames 10\nwait-jobs 10\n\
         assert-exists {apple}\nassert-exists {berry}\nassert-exists {cherry}\n\
         # With the filter cleared, the selection is the target again.\n\
         key Escape\nui browser assert-visible apple.txt, berry.txt, cherry.txt\n\
         assert-selection cherry.txt\nkey Delete\nwait-jobs 20\nwait-no-entry cherry.txt\n\
         assert-missing {cherry}\nassert-exists {berry}\nexit\n",
        work = shown(&work),
        apple = shown(&work.join("apple.txt")),
        berry = shown(&work.join("berry.txt")),
        cherry = shown(&work.join("cherry.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn delete_selects_the_next_item_in_details_view() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         click-entry b.txt\nkey Delete\nwait-jobs 20\nwait-no-entry b.txt | 10\n\
         assert-selection c.txt\nui browser assert-cursor c.txt\n\
         # The next delete needs no click: the selection moved on with the cursor.\n\
         key Delete\nwait-jobs 20\nwait-no-entry c.txt | 10\n\
         assert-missing {c}\nassert-selection d.txt\nui browser assert-cursor d.txt\nexit\n",
        work = shown(&work),
        c = shown(&work.join("c.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn delete_selects_the_next_item_in_grid_view() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewGrid\nwait-frames 5\n\
         # The last item has nothing after it: the one before it is selected.\n\
         click-entry e.txt\nkey Delete\nwait-jobs 20\nwait-no-entry e.txt | 10\n\
         assert-selection d.txt\nui browser assert-cursor d.txt\nexit\n",
        work = shown(&work),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn deleting_several_items_selects_the_one_after_them() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         select b.txt, c.txt\nassert-selected 2\nkey Delete\nwait-jobs 20\n\
         wait-no-entry b.txt | 10\nwait-no-entry c.txt | 10\n\
         assert-selection d.txt\nassert-selected 1\nexit\n",
        work = shown(&work),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn permanent_delete_selects_the_next_item_and_cancel_keeps_the_selection() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt", "d.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         select b.txt\ncmd DeletePermanently\ncancel\nwait-frames 10\n\
         # Cancelled: nothing is removed and the selection stays on b.txt.\n\
         assert-selection b.txt\nassert-entry b.txt\n\
         cmd DeletePermanently\naccept\nwait-jobs 20\nwait-no-entry b.txt | 10\n\
         assert-missing {b}\nassert-selection c.txt\nexit\n",
        work = shown(&work),
        b = shown(&work.join("b.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn moving_items_away_selects_the_next_item() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        write_file(&src.join(name), name.as_bytes());
    }
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "select b.txt",
            "cmd MoveToOtherPane",
            "wait-jobs 20",
            "wait-no-entry b.txt | 10",
            // Focus is on the source pane (left), so the selection read is its selection.
            "assert-selection c.txt",
        ],
    );
    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);
    expect_ok(&outcome);
    assert!(dst.join("b.txt").exists(), "the moved file must arrive");
    assert!(!src.join("b.txt").exists(), "the source must be gone");
}

#[test]
fn rename_with_the_selection_filtered_out_renames_nothing() {
    let (temp, work, profile) = list_fixture(&["apple.txt", "berry.txt", "cherry.txt"]);
    write_file(
        &profile.join("config").join("settings.json"),
        br#"{ "list_typing": "Filter" }"#,
    );
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         # Select cherry, then type a filter that hides it: F2 must not offer to rename berry.\n\
         click-entry cherry.txt\ntext berr\nui browser assert-visible berry.txt\n\
         key F2\nwait-frames 5\nui browser assert-no-dialog\n\
         assert-exists {berry}\nassert-exists {cherry}\n\
         # With the filter cleared, F2 renames the selected file.\n\
         key Escape\nassert-selection cherry.txt\n\
         key F2\ntype renamed.txt\naccept\nwait-exists {renamed}\n\
         assert-missing {cherry}\nassert-exists {berry}\nexit\n",
        work = shown(&work),
        berry = shown(&work.join("berry.txt")),
        cherry = shown(&work.join("cherry.txt")),
        renamed = shown(&work.join("renamed.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn arrows_move_the_file_cursor_after_a_toolbar_toggle_was_clicked() {
    // Three tiles, so they share one row even in a narrow window.
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt"]);
    let script = format!(
        "navigate {work}\ncmd TogglePreview\ncmd ViewGrid\nwait-frames 5\n\
         click-entry a.txt\nui browser assert-cursor a.txt\n\
         # Clicking the toggle leaves egui focus on it; the arrows must still move the cursor.\n\
         ui browser click-control group-similar\nui browser assert-grouping on\n\
         key ArrowRight\nui browser assert-cursor b.txt\n\
         # Down from the only row stops on the last tile instead of leaving the grid.\n\
         key ArrowDown\nui browser assert-cursor c.txt\n\
         key ArrowRight\nui browser assert-cursor c.txt\n\
         key Home\nkey ArrowLeft\nui browser assert-cursor a.txt\n\
         ui browser assert-grouping on\n\
         # Details view: Up on the first row must not send focus to the toolbar.\n\
         cmd ViewDetails\nwait-frames 3\nclick-entry a.txt\n\
         key ArrowUp\nui browser assert-cursor a.txt\n\
         key ArrowDown\nui browser assert-cursor b.txt\n\
         key ArrowDown\nui browser assert-cursor c.txt\n\
         key End\nkey ArrowDown\nui browser assert-cursor c.txt\n\
         key ArrowUp\nui browser assert-cursor b.txt\nexit\n",
        work = shown(&work),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn after_switching_panes_the_arrows_move_the_active_panes_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    for name in ["l1.txt", "l2.txt", "l3.txt"] {
        write_file(&left.join(name), b"x");
    }
    for name in ["r1.txt", "r2.txt", "r3.txt"] {
        write_file(&right.join(name), b"x");
    }
    let script = dual_pane_script(
        &left,
        &right,
        &[
            "cmd ViewDetails",
            "wait-frames 5",
            // A left row gets focus, then the right pane becomes active without a click (as
            // a command does); the left row keeps egui focus.
            "click-entry l1.txt",
            "key ArrowDown",
            "ui browser assert-cursor l2.txt",
            "focus right",
            "wait-frames 2",
            "assert-path RIGHT",
            "key Home",
            "key ArrowDown",
            "ui browser assert-cursor r2.txt",
            "key ArrowDown",
            "ui browser assert-cursor r3.txt",
            // And back with Tab: the left pane's cursor is where it was and moves again.
            "key Tab",
            "wait-frames 2",
            "key ArrowUp",
            "ui browser assert-cursor l1.txt",
        ],
    )
    .replace("assert-path RIGHT", &format!("assert-path {}", shown(&right)));
    let outcome = run_app(temp.path(), &temp.path().join("profile"), &script);
    expect_ok(&outcome);
}

#[test]
fn delete_after_closing_the_folder_of_the_selection_touches_no_file() {
    let (temp, work, profile) =
        list_fixture(&["Shoot/img_01.txt", "Shoot/img_02.txt", "notes.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         ui browser click-arrow Shoot\nwait-entry img_01.txt\nwait-frames 3\n\
         cmd ToggleStickySelection\nclick-entry img_01.txt\nclick-entry img_02.txt\n\
         assert-selection img_01.txt, img_02.txt\n\
         # Closing the folder hides the selection: Delete must not take the folder instead.\n\
         ui browser click-arrow Shoot\nui browser assert-collapsed-folder Shoot\n\
         key Delete\nwait-frames 10\nwait-jobs 10\nui browser assert-no-dialog\n\
         assert-toast Nothing to delete\n\
         assert-exists {shoot}\nassert-exists {img01}\nassert-exists {img02}\n\
         # Reopening shows the selection again, and Delete takes just those files.\n\
         ui browser click-arrow Shoot\nwait-entry img_01.txt\nwait-frames 3\n\
         assert-selection img_01.txt, img_02.txt\n\
         key Delete\nwait-jobs 20\nwait-no-entry img_02.txt\n\
         assert-missing {img01}\nassert-missing {img02}\nassert-exists {shoot}\nexit\n",
        work = shown(&work),
        shoot = shown(&work.join("Shoot")),
        img01 = shown(&work.join("Shoot").join("img_01.txt")),
        img02 = shown(&work.join("Shoot").join("img_02.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn right_click_on_a_selected_row_makes_it_the_single_item_target() {
    let (temp, work, profile) = list_fixture(&["a.txt", "b.txt", "c.txt"]);
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         # Sticky clicks select a and b, then leave the cursor on c (clicked on and off).\n\
         cmd ToggleStickySelection\nclick-entry a.txt\nclick-entry b.txt\n\
         click-entry c.txt\nclick-entry c.txt\n\
         ui browser assert-cursor c.txt\nassert-selection a.txt, b.txt\n\
         # The right-click on a keeps the selection and moves the cursor there.\n\
         right-click-entry a.txt\nwait-frames 2\n\
         ui browser assert-cursor a.txt\nassert-selection a.txt, b.txt\n\
         cmd Rename\ntype renamed.txt\naccept\nwait-exists {renamed}\n\
         assert-missing {a}\nassert-exists {b}\nassert-exists {c}\nexit\n",
        work = shown(&work),
        a = shown(&work.join("a.txt")),
        b = shown(&work.join("b.txt")),
        c = shown(&work.join("c.txt")),
        renamed = shown(&work.join("renamed.txt")),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
}

#[test]
fn right_click_in_the_other_split_pane_targets_the_clicked_tile() {
    let temp = tempfile::tempdir().unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    write_file(&left.join("l1.txt"), b"x");
    for name in ["r1.txt", "r2.txt", "r3.txt"] {
        write_file(&right.join(name), b"x");
    }
    let script = dual_pane_script(
        &left,
        &right,
        &[
            "focus right",
            "cmd ViewGrid",
            "wait-frames 5",
            "cmd ToggleStickySelection",
            "click-entry r1.txt",
            "click-entry r2.txt",
            "click-entry r3.txt",
            "click-entry r3.txt",
            "ui browser assert-cursor r3.txt",
            "right-click-entry r1.txt",
            "wait-frames 2",
            "ui browser assert-cursor r1.txt",
            "assert-selection r1.txt, r2.txt",
            "cmd Rename",
            "type renamed.txt",
            "accept",
            "wait-exists RENAMED",
            "assert-exists R2",
            "assert-exists R3",
        ],
    )
    .replace("RENAMED", &shown(&right.join("renamed.txt")))
    .replace("R2", &shown(&right.join("r2.txt")))
    .replace("R3", &shown(&right.join("r3.txt")));
    let outcome = run_app(temp.path(), &temp.path().join("profile"), &script);
    expect_ok(&outcome);
}

#[test]
fn folder_totals_appear_in_the_size_column() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let profile = temp.path().join("profile");
    let mib = vec![0u8; 1 << 20];
    write_file(&work.join("photos").join("top.bin"), &mib);
    write_file(&work.join("photos").join("sub").join("middle.bin"), &mib);
    write_file(
        &work
            .join("photos")
            .join("sub")
            .join("deeper")
            .join("deep.bin"),
        &mib,
    );
    fs::create_dir_all(work.join("empty")).unwrap();
    write_file(&work.join("note.txt"), b"hello");
    // Write paths are relative to the script sandbox, which is the test's temp folder.
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nwait-frames 5\n\
         ui browser assert-size photos | 3.00 MB\n\
         ui browser assert-size empty | 0 B\n\
         ui browser assert-size note.txt | 5 B\n\
         ui browser write-file work/photos/sub/deeper/extra.bin | 1048576\n\
         ui browser assert-size photos | 4.00 MB\n\
         exit\n",
        work = shown(&work),
    );
    let outcome = run_app(temp.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn a_large_folder_total_shows_quickly() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let profile = temp.path().join("profile");
    // 100 folders of 100 files, 1 KiB each: 10,000 files, 9.77 MB in all.
    for dir in 0..100 {
        for file in 0..100 {
            write_file(
                &work
                    .join("big")
                    .join(format!("dir-{dir:03}"))
                    .join(format!("file-{file:03}.bin")),
                &[0u8; 1024],
            );
        }
    }
    let script = format!(
        "navigate {work}\ncmd ViewDetails\nui browser assert-size big | 9.77 MB\nexit\n",
        work = shown(&work),
    );
    let started = std::time::Instant::now();
    let outcome = run_app(temp.path(), &profile, &script);
    let elapsed = started.elapsed();
    expect_ok(&outcome);
    assert_no_crash(&profile);
    eprintln!(
        "10,000-file folder: `big` showed 9.77 MB after {:.2} s (wall clock, app start-up included)",
        elapsed.as_secs_f64()
    );
}
