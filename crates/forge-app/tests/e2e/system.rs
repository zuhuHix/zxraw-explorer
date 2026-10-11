//! End-to-end scenarios: system.

#[allow(unused_imports)]
use crate::*;

#[cfg(windows)]
#[test]
fn heic_thumbnail_is_visible_in_grid_and_viewer() {
    let sandbox = tempfile::tempdir().unwrap();
    let media = sandbox.path().join("media");
    let profile = sandbox.path().join("profile");
    std::fs::create_dir_all(&media).unwrap();
    let fixture = media.join("generated.heic");
    match forge_media::wic::encode_test_fixture(&fixture, forge_media::wic::TestContainer::Heif) {
        Ok(()) => {}
        Err(forge_media::MediaError::Unsupported(reason)) => {
            eprintln!("skipped HEIC grid/viewer scenario: {reason}");
            return;
        }
        Err(error) => panic!("could not encode a WIC HEIC fixture: {error}"),
    }

    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let grid_shot = shots.join("heic-grid.png");
    let viewer_shot = shots.join("heic-viewer.png");
    let script = format!(
        "limit 120\ndismiss-notices\nnavigate {}\ncmd ViewGrid\n\
         ui system wait-thumbnail generated.heic | 45\n\
         ui system assert-thumbnail-ready generated.heic\n\
         screenshot {}\nselect generated.heic\ncmd Open\n\
         ui viewer assert-open single\nui viewer assert-current generated.heic\n\
         ui viewer wait-decoded 45\nscreenshot {}\nexit\n",
        shown(&media),
        shown(&grid_shot),
        shown(&viewer_shot),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        grid_shot.is_file(),
        "missing grid screenshot: {}",
        grid_shot.display()
    );
    assert!(
        viewer_shot.is_file(),
        "missing viewer screenshot: {}",
        viewer_shot.display()
    );
}

#[test]
fn corrupt_heic_shows_thumbnail_and_viewer_errors_without_crashing() {
    let sandbox = tempfile::tempdir().unwrap();
    let media = sandbox.path().join("media");
    let profile = sandbox.path().join("profile");
    std::fs::create_dir_all(&media).unwrap();
    write_file(&media.join("corrupt.heic"), &noise(4096, 0x41C0));

    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let grid_shot = shots.join("corrupt-heic-grid.png");
    let viewer_shot = shots.join("corrupt-heic-viewer.png");
    let script = format!(
        "limit 120\ndismiss-notices\nnavigate {}\ncmd ViewGrid\n\
         ui system wait-thumbnail corrupt.heic | 45\n\
         ui system assert-thumbnail-error corrupt.heic\nscreenshot {}\n\
         select corrupt.heic\ncmd Open\nui viewer assert-open single\n\
         ui viewer assert-current corrupt.heic\nui viewer wait-decoded 45\n\
         ui viewer assert-error-shown\nscreenshot {}\nexit\n",
        shown(&media),
        shown(&grid_shot),
        shown(&viewer_shot),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        grid_shot.is_file(),
        "missing grid screenshot: {}",
        grid_shot.display()
    );
    assert!(
        viewer_shot.is_file(),
        "missing viewer screenshot: {}",
        viewer_shot.display()
    );
}
