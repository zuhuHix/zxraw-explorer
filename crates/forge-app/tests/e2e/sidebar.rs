//! End-to-end scenarios: sidebar (Quick access pins, saved layouts, section headers and menus).

#[allow(unused_imports)]
use crate::*;

use std::fs;
use std::path::Path;

/// Settings file of `profile` as JSON (written on exit).
fn settings_json(profile: &Path) -> serde_json::Value {
    let path = profile.join("config").join("settings.json");
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).expect("settings.json is JSON")
}

/// Drops the saved tabs so the next run starts in one fresh tab (the folders of the previous
/// run may be gone by then).
fn forget_session(profile: &Path) {
    let _ = fs::remove_file(profile.join("config").join("session.json"));
}

#[test]
fn pins_are_added_reordered_renamed_and_removed() {
    let sandbox = tempfile::tempdir().unwrap();
    let docs = sandbox.path().join("docs");
    let photos = sandbox.path().join("photos");
    write_file(&docs.join("report.txt"), b"quarterly report\n");
    fs::create_dir_all(&photos).unwrap();
    let profile = sandbox.path().join("profile");

    let script = format!(
        "dismiss-notices\n\
         navigate {docs}\nwait-entry report.txt\nui sidebar pins\n\
         select report.txt\ncmd PinToQuickAccess\nui sidebar pins report.txt\n\
         navigate {photos}\ncmd BookmarkFolder\nui sidebar pins report.txt, photos\n\
         ui sidebar move-pin 2 1\nui sidebar pins photos, report.txt\n\
         ui sidebar rename-pin photos | Pictures\nui sidebar pins Pictures, report.txt\n\
         ui sidebar unpin report.txt\nui sidebar pins Pictures\n\
         ui sidebar unpin Pictures\nui sidebar pins\n\
         assert-no-error-toast\nexit\n",
        docs = shown(&docs),
        photos = shown(&photos),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn pin_to_a_deleted_file_reports_missing_and_folder_pins_still_navigate() {
    let sandbox = tempfile::tempdir().unwrap();
    let docs = sandbox.path().join("docs");
    let photos = sandbox.path().join("photos");
    let ghost = docs.join("ghost.txt");
    write_file(&ghost, b"soon gone\n");
    fs::create_dir_all(&photos).unwrap();
    let profile = sandbox.path().join("profile");

    let script = format!(
        "dismiss-notices\nnavigate {docs}\n\
         ui sidebar pin-file {ghost}\nui sidebar pin {photos}\n\
         ui sidebar wait-pin-status ghost.txt present | 20\n\
         ui sidebar wait-pin-status photos present | 20\n\
         ui tools external delete {ghost}\n\
         ui sidebar wait-pin-status ghost.txt missing | 20\n\
         ui sidebar click-pin ghost.txt\n\
         assert-toast ghost.txt is not available\n\
         assert-path {docs}\n\
         navigate {sandbox}\n\
         ui sidebar click-pin photos\n\
         assert-path {photos}\nexit\n",
        docs = shown(&docs),
        photos = shown(&photos),
        ghost = shown(&ghost),
        sandbox = shown(sandbox.path()),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn pins_persist_across_runs_with_the_same_profile() {
    let sandbox = tempfile::tempdir().unwrap();
    let docs = sandbox.path().join("docs");
    let photos = sandbox.path().join("photos");
    write_file(&docs.join("report.txt"), b"quarterly report\n");
    fs::create_dir_all(&photos).unwrap();
    let report = docs.join("report.txt");
    let profile = sandbox.path().join("profile");

    let first = format!(
        "dismiss-notices\nnavigate {docs}\n\
         ui sidebar pin-file {report}\nui sidebar pin {photos}\n\
         ui sidebar rename-pin photos | Pictures\nui sidebar pins report.txt, Pictures\nexit\n",
        docs = shown(&docs),
        report = shown(&report),
        photos = shown(&photos),
    );
    let outcome = run_app(sandbox.path(), &profile, &first);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let saved = settings_json(&profile).to_string();
    assert!(
        saved.contains("Pictures") && saved.contains("report.txt"),
        "the pins were not saved on exit: {saved}"
    );

    let second = "dismiss-notices\nui sidebar pins report.txt, Pictures\n\
                  ui sidebar wait-pin-status Pictures present | 20\nexit\n";
    let outcome = run_app(sandbox.path(), &profile, second);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn old_folder_bookmarks_show_up_as_pins() {
    let sandbox = tempfile::tempdir().unwrap();
    let photos = sandbox.path().join("photos");
    fs::create_dir_all(&photos).unwrap();
    let profile = sandbox.path().join("profile");
    let legacy = serde_json::json!({ "bookmarks": [photos.display().to_string()] });
    write_file(
        &profile.join("config").join("settings.json"),
        legacy.to_string().as_bytes(),
    );

    let script = format!(
        "dismiss-notices\nui sidebar pins photos\n\
         ui sidebar wait-pin-status photos present | 20\n\
         ui sidebar click-pin photos\nassert-path {photos}\nexit\n",
        photos = shown(&photos),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);

    let saved = settings_json(&profile);
    let bookmarks_left = saved["bookmarks"].as_array().is_none_or(|a| a.is_empty());
    assert!(
        bookmarks_left,
        "the old bookmarks were not moved over: {saved}"
    );
}

#[test]
fn layouts_save_load_and_keep_one_entry_per_name() {
    let sandbox = tempfile::tempdir().unwrap();
    let alpha = sandbox.path().join("alpha");
    let beta = sandbox.path().join("beta");
    let gamma = sandbox.path().join("gamma");
    for dir in [&alpha, &beta, &gamma] {
        fs::create_dir_all(dir).unwrap();
    }
    let profile = sandbox.path().join("profile");

    // Tab 2 is the one saved: split, with the right pane on beta and the left in grid view.
    let script = format!(
        "dismiss-notices\nnavigate {alpha}\n\
         cmd NewTab\nnavigate {alpha}\ncmd ToggleDualPane\nfocus right\nnavigate {beta}\n\
         focus left\ncmd ViewGrid\nui sidebar assert-view grid\n\
         cmd SaveLayout\ntype Culling\naccept\nui sidebar layouts Culling\n\
         cmd SaveLayout\ntype Culling\naccept\nui sidebar layouts Culling\n\
         navigate {gamma}\ncmd ToggleDualPane\ncmd ViewDetails\n\
         ui sidebar assert-split off\nui sidebar assert-view details\n\
         ui sidebar load-layout Culling\n\
         assert-tabs 2\nui sidebar assert-split on\n\
         focus left\nassert-path {alpha}\nui sidebar assert-view grid\n\
         focus right\nassert-path {beta}\nui sidebar assert-view details\n\
         assert-no-error-toast\nexit\n",
        alpha = shown(&alpha),
        beta = shown(&beta),
        gamma = shown(&gamma),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let layouts = settings_json(&profile)["layouts"].clone();
    assert_eq!(
        layouts.as_array().map(Vec::len),
        Some(1),
        "saving the same name twice must keep one layout: {layouts}"
    );
}

#[test]
fn empty_sections_start_collapsed_and_their_menus_open_the_layout_and_collection_windows() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");

    // A fresh profile has no layouts or collections: both sections start collapsed. Every
    // action is reached with real clicks on the header and its "⋯" menu. The collection
    // windows open over the sidebar, so each run opens only one of them, last.
    let script = "dismiss-notices\n\
         ui sidebar assert-section layouts closed\n\
         ui sidebar assert-section collections closed\n\
         ui browser click-control sidebar-layouts\n\
         ui sidebar assert-section layouts open\n\
         ui browser click-control sidebar-layouts-menu\n\
         ui system click-menu Save current layout…\n\
         type Culling\naccept\nui sidebar layouts Culling\n\
         ui browser click-control sidebar-layouts-menu\n\
         ui system click-menu Manage layouts…\n\
         ui sidebar assert-window manage-layouts open\n\
         ui browser click-control sidebar-collections-menu\n\
         ui system click-menu New collection…\n\
         ui sidebar assert-window new-collection open\n\
         ui sidebar assert-section collections closed\n\
         assert-no-error-toast\nexit\n";
    let outcome = run_app(sandbox.path(), &profile, script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let settings = settings_json(&profile);
    assert_eq!(settings["sidebar_sections"]["layouts"], true, "{settings}");
    assert!(
        settings["sidebar_sections"].get("collections").is_none(),
        "{settings}"
    );

    // The choice is remembered; collapsing a section with items is remembered too.
    // "Manage collections…" opens the Collections window.
    forget_session(&profile);
    let script = "dismiss-notices\n\
         ui sidebar assert-section layouts open\n\
         ui browser click-control sidebar-layouts\n\
         ui sidebar assert-section layouts closed\n\
         ui browser click-control sidebar-collections-menu\n\
         ui system click-menu Manage collections…\n\
         ui sidebar assert-window collections open\n\
         assert-no-error-toast\nexit\n";
    let outcome = run_app(sandbox.path(), &profile, script);
    expect_ok(&outcome);
    assert_eq!(
        settings_json(&profile)["sidebar_sections"]["layouts"],
        false
    );
}

#[test]
fn layout_with_a_missing_folder_opens_the_rest_and_says_so() {
    let sandbox = tempfile::tempdir().unwrap();
    let one = sandbox.path().join("one");
    let two = sandbox.path().join("two");
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    let profile = sandbox.path().join("profile");

    let save = format!(
        "dismiss-notices\nnavigate {one}\ncmd NewTab\nnavigate {two}\n\
         cmd SaveLayout\ntype Partial\naccept\nui sidebar layouts Partial\nexit\n",
        one = shown(&one),
        two = shown(&two),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &save));
    assert_no_crash(&profile);

    // Delete one folder between the runs. Without the saved session the next run starts in
    // one tab, which the layout then replaces.
    fs::remove_dir_all(&two).unwrap();
    forget_session(&profile);

    let load = format!(
        "dismiss-notices\nui sidebar load-layout Partial\n\
         assert-toast not found\nassert-tabs 1\nassert-path {one}\nexit\n",
        one = shown(&one),
    );
    let outcome = run_app(sandbox.path(), &profile, &load);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn layout_whose_folders_are_all_missing_keeps_the_open_tabs() {
    let sandbox = tempfile::tempdir().unwrap();
    let one = sandbox.path().join("one");
    let two = sandbox.path().join("two");
    let keep = sandbox.path().join("keep");
    for dir in [&one, &two, &keep] {
        fs::create_dir_all(dir).unwrap();
    }
    let profile = sandbox.path().join("profile");

    let save = format!(
        "dismiss-notices\nnavigate {one}\ncmd NewTab\nnavigate {two}\n\
         cmd SaveLayout\ntype Both\naccept\nui sidebar layouts Both\nexit\n",
        one = shown(&one),
        two = shown(&two),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &save));
    assert_no_crash(&profile);

    fs::remove_dir_all(&one).unwrap();
    fs::remove_dir_all(&two).unwrap();
    forget_session(&profile);

    let load = format!(
        "dismiss-notices\nnavigate {keep}\n\
         ui sidebar load-layout Both\n\
         assert-toast none of its folders were found\n\
         assert-tabs 1\nassert-path {keep}\nexit\n",
        keep = shown(&keep),
    );
    let outcome = run_app(sandbox.path(), &profile, &load);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn network_locations_are_added_renamed_and_removed_without_freezing_on_an_offline_share() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");

    // 192.0.2.1 is TEST-NET-1 (RFC 5737): never routable, so the share never answers and
    // every check of it runs into the probe's timeout.
    let script = format!(
        "dismiss-notices\nnavigate {sandbox}\nui sidebar net\n\
         ui sidebar net-add-fails \\\\nas\n\
         ui sidebar net-add-fails C:\\photos\n\
         ui sidebar net-add \\\\192.0.2.1\\x | Lab NAS\n\
         ui sidebar net-add-fails //192.0.2.1/X/\n\
         ui sidebar net Lab NAS\n\
         ui sidebar responsive 30 | 1000\n\
         ui sidebar wait-net-status Lab NAS offline | 60\n\
         ui sidebar net-click Lab NAS\n\
         assert-toast Lab NAS is offline or unreachable\n\
         assert-path {sandbox}\n\
         ui sidebar responsive 30 | 1000\n\
         ui sidebar net-rename Lab NAS | Studio share\nui sidebar net Studio share\n\
         exit\n",
        sandbox = shown(sandbox.path()),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let saved = settings_json(&profile)["network_locations"].clone();
    assert_eq!(saved[0]["path"], r"\\192.0.2.1\x", "{saved}");
    assert_eq!(saved[0]["name"], "Studio share", "{saved}");

    let second = "dismiss-notices\nui sidebar net Studio share\n\
                  ui sidebar net-remove Studio share\nui sidebar net\nexit\n";
    let outcome = run_app(sandbox.path(), &profile, second);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let saved = settings_json(&profile)["network_locations"].clone();
    assert_eq!(saved.as_array().map(Vec::len), Some(0), "{saved}");
}

#[test]
fn going_to_an_offline_share_path_never_waits_on_the_ui_thread() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    // TEST-NET-1 again: every file call on these paths hangs until the network gives up.
    let script = format!(
        "dismiss-notices\nnavigate {sandbox}\n\
         ui sidebar go \\\\192.0.2.1\\x\\shoot\\a.jpg\n\
         ui sidebar responsive 30 | 1000\n\
         ui sidebar go \\\\192.0.2.1\\x\n\
         ui sidebar responsive 30 | 1000\n\
         navigate {sandbox}\nassert-path {sandbox}\nexit\n",
        sandbox = shown(sandbox.path()),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}
