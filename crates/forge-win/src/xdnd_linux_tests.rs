//! Tests of the XDND module: the pure rules, and real drags on a private Xvfb (ignored by
//! default: `cargo test -p forge-win xdnd -- --ignored`).

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::drop_target::resolve_effect;

fn p(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

// ---- Rules ----

#[test]
fn uri_lists_round_trip_spaces_percent_and_non_utf8_bytes() {
    let paths = vec![
        p(b"/tmp/a b%.txt"),
        p(b"/tmp/caf\xc3\xa9 \xff.bin"),
        p(b"/tmp/plain-name_1.~x"),
    ];
    let list = formats::uri_list(&paths);
    assert_eq!(
        list,
        b"file:///tmp/a%20b%25.txt\r\nfile:///tmp/caf%C3%A9%20%FF.bin\r\nfile:///tmp/plain-name_1.~x"
    );
    assert_eq!(formats::parse_uri_list(&list), paths);
    // A trailing CRLF, LF separators, comments, localhost and foreign URIs.
    let mixed = b"# a comment\r\nfile://localhost/tmp/a%20b%25.txt\nhttp://example.com/x\r\n\
                  file://otherhost/tmp/x\r\nfile:///tmp/caf%C3%A9%20%FF.bin\r\n";
    assert_eq!(formats::parse_uri_list(mixed), paths[..2].to_vec());
    // Nothing is canonicalized: dot segments and symlink names stay as given.
    assert_eq!(
        formats::parse_uri_list(b"file:///tmp/../tmp/link"),
        vec![p(b"/tmp/../tmp/link")]
    );
}

#[test]
fn modifiers_pick_the_suggested_action_and_what_is_offered() {
    assert_eq!(action_for_keys(false, false), Action::Copy);
    assert_eq!(action_for_keys(true, false), Action::Copy);
    assert_eq!(action_for_keys(false, true), Action::Move);
    assert_eq!(action_for_keys(true, true), Action::Link);
    // Move is offered only when asked for.
    assert_eq!(offered_actions(Action::Copy), &[Action::Copy, Action::Link]);
    assert_eq!(offered_actions(Action::Move), &[Action::Move]);
    assert_eq!(offered_actions(Action::Link), &[Action::Link]);
}

#[test]
fn the_answer_is_limited_to_what_the_source_allows() {
    let all = allowed_effects(&[Action::Copy, Action::Move, Action::Link], None);
    assert_eq!(all, EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK);
    let resolve = |answer, allowed| status_action(resolve_effect(answer, allowed, false));
    assert_eq!(resolve(DropEffect::Move, all), Some(Action::Move));
    assert_eq!(resolve(DropEffect::Copy, all), Some(Action::Copy));
    assert_eq!(resolve(DropEffect::None, all), None);
    // Move not allowed: Copy if that is.
    let copy_link = allowed_effects(&[Action::Copy, Action::Link], Some(Action::Copy));
    assert_eq!(resolve(DropEffect::Move, copy_link), Some(Action::Copy));
    let link_only = allowed_effects(&[Action::Link], Some(Action::Link));
    assert_eq!(resolve(DropEffect::Move, link_only), None);
    assert_eq!(resolve(DropEffect::Copy, link_only), None);
    // No list: the suggested action, plus Copy (XDND's default).
    let suggested_move = allowed_effects(&[], Some(Action::Move));
    assert_eq!(suggested_move, EFFECT_MOVE | EFFECT_COPY);
    assert_eq!(
        resolve(DropEffect::Move, suggested_move),
        Some(Action::Move)
    );
    let suggested_private = allowed_effects(&[Action::Ask], Some(Action::Private));
    assert_eq!(suggested_private, EFFECT_COPY);
    assert_eq!(
        resolve(DropEffect::Move, suggested_private),
        Some(Action::Copy)
    );
}

#[test]
fn finished_reports_a_move_as_private_never_as_move() {
    assert_eq!(finished_action(DropEffect::Copy), Some(Action::Copy));
    assert_eq!(finished_action(DropEffect::Move), Some(Action::Private));
    assert_eq!(finished_action(DropEffect::None), None);
    for effect in [DropEffect::None, DropEffect::Copy, DropEffect::Move] {
        assert_ne!(finished_action(effect), Some(Action::Move));
    }
}

#[test]
fn feedback_and_results_follow_the_target() {
    assert_eq!(
        feedback_of(false, Some(Action::Copy)),
        DragFeedback::Refused
    );
    assert_eq!(feedback_of(true, Some(Action::Move)), DragFeedback::Move);
    assert_eq!(feedback_of(true, Some(Action::Link)), DragFeedback::Link);
    assert_eq!(feedback_of(true, Some(Action::Private)), DragFeedback::Copy);
    assert_eq!(feedback_of(true, None), DragFeedback::Copy);

    assert_eq!(
        drop_result_of(false, Some(Action::Move), 7),
        DropResult::None
    );
    assert_eq!(
        drop_result_of(true, Some(Action::Copy), 7),
        DropResult::Copy
    );
    assert_eq!(
        drop_result_of(true, Some(Action::Move), 7),
        DropResult::Move
    );
    assert_eq!(
        drop_result_of(true, Some(Action::Link), 7),
        DropResult::Link
    );
    assert_eq!(drop_result_of(true, None, NONE), DropResult::Copy);
    assert_eq!(
        drop_result_of(true, Some(Action::Private), 9),
        DropResult::Other(9)
    );

    assert_eq!(
        test_result_of(true, Some(Action::Private), 9),
        DropResult::Move
    );
    assert_eq!(
        test_result_of(true, Some(Action::Copy), 9),
        DropResult::Copy
    );
    assert_eq!(
        test_result_of(true, Some(Action::Move), 9),
        DropResult::Other(9)
    );
    assert_eq!(
        test_result_of(false, Some(Action::Copy), 9),
        DropResult::None
    );
}

#[test]
fn escape_is_read_from_the_keymap_bits() {
    let mut keymap = [0u8; 32];
    keymap[9 / 8] |= 1 << (9 % 8);
    assert!(key_down(&keymap, Some(9)));
    assert!(!key_down(&keymap, Some(10)));
    assert!(!key_down(&keymap, None));
}

// ---- Real drags, on a private Xvfb (never the user's display) ----

/// Serializes these tests (each starts its own server; one at a time keeps them light).
static XVFB_LOCK: Mutex<()> = Mutex::new(());

struct X {
    display: crate::desktop::PrivateDisplay,
    _lock: MutexGuard<'static, ()>,
}

impl X {
    fn start() -> Self {
        let lock = lock(&XVFB_LOCK);
        let display = crate::desktop::PrivateDisplay::start(400, 300).expect("start Xvfb");
        Self {
            display,
            _lock: lock,
        }
    }

    fn name(&self) -> &str {
        self.display.display()
    }
}

/// A control connection: windows, and input through XTEST.
struct Ctl {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

impl Ctl {
    fn new(display: &str) -> Self {
        let (conn, root) = connect(display).unwrap();
        let atoms = Atoms::intern(&conn).unwrap();
        Self { conn, root, atoms }
    }

    /// A mapped window; with `aware`, `XdndAware` = 5 (a stand-in for winit's toplevel).
    fn window(&self, x: i16, y: i16, w: u16, h: u16, aware: bool) -> Window {
        let window = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                x,
                y,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .unwrap();
        if aware {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    self.atoms.aware,
                    AtomEnum::ATOM,
                    &[XDND_VERSION],
                )
                .unwrap();
        }
        self.conn.map_window(window).unwrap();
        self.conn.sync().unwrap();
        window
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        use x11rb::protocol::xtest::ConnectionExt as _;
        self.conn
            .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)
            .unwrap();
        self.conn.sync().unwrap();
    }

    fn motion(&self, x: i16, y: i16) {
        self.fake(6, 0, x, y); // MotionNotify, absolute
    }

    fn button1(&self, down: bool) {
        self.fake(if down { 4 } else { 5 }, 1, 0, 0);
    }

    fn key(&self, keysym: u32, down: bool) {
        let code = keycode_of(&self.conn, keysym).expect("keysym on the keyboard");
        self.fake(if down { 2 } else { 3 }, code, 0, 0);
    }

    fn proxy_of(&self, window: Window) -> Option<Window> {
        property32(
            &self.conn,
            window,
            self.atoms.proxy,
            AtomEnum::WINDOW.into(),
        )?
        .first()
        .copied()
    }
}

/// What the test drop target saw.
#[derive(Debug, Default)]
struct TargetLog {
    /// (version, types) of each XdndEnter.
    enters: Vec<(u32, Vec<Atom>)>,
    /// (suggested action, the source's XdndActionList) of each XdndPosition.
    positions: Vec<(Atom, Vec<Atom>)>,
    leaves: usize,
    drops: usize,
    data: Option<Vec<u8>>,
    targets: Vec<Atom>,
    delete_refused: Option<bool>,
}

/// Converts `XdndSelection` to `target` on `window`: `None` when refused or late.
fn convert(
    conn: &RustConnection,
    atoms: &Atoms,
    window: Window,
    target: Atom,
    time: u32,
) -> Option<Vec<u8>> {
    conn.convert_selection(window, atoms.selection, target, atoms.data0, time)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(event) = next_event(conn, Some(deadline)).unwrap() {
        if let Event::SelectionNotify(e) = event
            && e.requestor == window
            && e.target == target
        {
            if e.property == NONE {
                return None;
            }
            let reply = conn
                .get_property(true, window, atoms.data0, AtomEnum::ANY, 0, u32::MAX / 4)
                .unwrap()
                .reply()
                .unwrap();
            return Some(reply.value);
        }
    }
    None
}

/// A mapped XDND target (version 5) covering the rectangle, served by a thread until it
/// finished a drop (with `finish` as the action) or saw a Leave.
fn spawn_target(
    display: &str,
    rect: (i16, i16, u16, u16),
    finish: Action,
) -> (Window, std::thread::JoinHandle<TargetLog>) {
    spawn_target_finishing(display, rect, Some(finish))
}

/// [`spawn_target`] finishing with `finish`, or with `None` the action of the last position
/// (as a file manager does what was suggested last).
fn spawn_target_finishing(
    display: &str,
    rect: (i16, i16, u16, u16),
    finish: Option<Action>,
) -> (Window, std::thread::JoinHandle<TargetLog>) {
    let ctl = Ctl::new(display);
    let window = ctl.window(rect.0, rect.1, rect.2, rect.3, true);
    let thread = std::thread::spawn(move || {
        let Ctl { conn, atoms, .. } = ctl;
        let mut log = TargetLog::default();
        let deadline = Instant::now() + Duration::from_secs(20);
        while let Some(event) = next_event(&conn, Some(deadline)).unwrap() {
            let Event::ClientMessage(e) = event else {
                continue;
            };
            let l = e.data.as_data32();
            if e.type_ == atoms.enter {
                let types = if l[1] & 1 != 0 {
                    property32(&conn, l[0], atoms.type_list, AtomEnum::ATOM.into())
                        .unwrap_or_default()
                } else {
                    l[2..].iter().copied().filter(|&t| t != NONE).collect()
                };
                log.enters.push((l[1] >> 24, types));
            } else if e.type_ == atoms.position {
                let list = property32(&conn, l[0], atoms.action_list, AtomEnum::ATOM.into())
                    .unwrap_or_default();
                log.positions.push((l[4], list));
                send_message(&conn, l[0], l[0], atoms.status, [window, 1 | 2, 0, 0, l[4]]);
                conn.flush().unwrap();
            } else if e.type_ == atoms.leave {
                log.leaves += 1;
                return log;
            } else if e.type_ == atoms.drop {
                log.drops += 1;
                log.data = convert(&conn, &atoms, window, atoms.uri_list, l[2]);
                log.targets = convert(&conn, &atoms, window, atoms.targets, l[2])
                    .map(|bytes| {
                        bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| u32::from_ne_bytes(*c))
                            .collect()
                    })
                    .unwrap_or_default();
                log.delete_refused =
                    Some(convert(&conn, &atoms, window, atoms.delete, l[2]).is_none());
                send_message(
                    &conn,
                    l[0],
                    l[0],
                    atoms.finished,
                    [
                        window,
                        1,
                        finish.map_or_else(
                            || log.positions.last().map_or(NONE, |(action, _)| *action),
                            |finish| atoms.action(finish),
                        ),
                        0,
                        0,
                    ],
                );
                conn.sync().unwrap();
                return log;
            }
        }
        log
    });
    (window, thread)
}

fn wait_result(session: &DragOutSession, limit: Duration) -> Result<DropResult> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(result) = session.poll() {
            return result;
        }
        assert!(Instant::now() < deadline, "the drag did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Files with a space, `%` and non-UTF-8 bytes in their names, and their `text/uri-list`.
fn test_files(dir: &Path) -> (Vec<PathBuf>, Vec<u8>) {
    let base = dir.as_os_str().as_bytes();
    assert!(
        base.iter()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-~".contains(b)),
        "the temp dir needs no encoding: {}",
        dir.display()
    );
    let paths = vec![
        dir.join("a b%.txt"),
        dir.join(OsStr::from_bytes(b"caf\xc3\xa9 \xff.bin")),
    ];
    for path in &paths {
        std::fs::write(path, b"x").unwrap();
    }
    let dir = dir.display();
    let list =
        format!("file://{dir}/a%20b%25.txt\r\nfile://{dir}/caf%C3%A9%20%FF.bin").into_bytes();
    (paths, list)
}

fn items(paths: &[PathBuf]) -> Vec<DragItem> {
    paths.iter().cloned().map(DragItem::unknown).collect()
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_copies_over_a_target_and_never_over_our_window() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let (_target, log) = spawn_target(x.name(), (0, 0, 200, 300), Action::Copy);
    let own = ctl.window(200, 0, 200, 300, true);
    let dir = tempfile::tempdir().unwrap();
    let (paths, expected) = test_files(dir.path());

    ctl.motion(300, 150);
    ctl.button1(true);
    assert_eq!(
        pointer_on(x.name(), own),
        Some(PointerState {
            x: 100,
            y: 150,
            ctrl: false,
            shift: false,
            button1: true
        })
    );
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    // Our own window (XdndAware, like winit's) is never a target.
    assert_eq!(session.feedback(), DragFeedback::NoTarget);
    assert!(session.poll().is_none());

    ctl.motion(100, 100);
    std::thread::sleep(Duration::from_millis(150));
    ctl.motion(110, 120);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(session.feedback(), DragFeedback::Copy);
    ctl.button1(false);
    assert_eq!(
        wait_result(&session, Duration::from_secs(5)).unwrap(),
        DropResult::Copy
    );

    let log = log.join().unwrap();
    let atoms = &ctl.atoms;
    assert_eq!(log.enters.len(), 1, "{log:?}");
    assert_eq!(log.enters[0].0, 5);
    assert_eq!(
        log.enters[0].1,
        vec![
            atoms.uri_list,
            atoms.gnome,
            atoms.utf8_string,
            atoms.text_plain_utf8,
            atoms.text_plain
        ]
    );
    assert!(log.positions.len() >= 2, "{log:?}");
    for (action, list) in &log.positions {
        assert_eq!(*action, atoms.copy);
        assert_eq!(list, &vec![atoms.copy, atoms.link]);
    }
    assert_eq!((log.leaves, log.drops), (0, 1));
    assert_eq!(log.data.as_deref(), Some(expected.as_slice()));
    assert!(log.targets.contains(&atoms.uri_list) && !log.targets.contains(&atoms.delete));
    assert_eq!(log.delete_refused, Some(true));
    assert!(paths.iter().all(|p| p.exists()));
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_with_shift_offers_only_move() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let (_target, log) = spawn_target(x.name(), (0, 0, 200, 300), Action::Move);
    let own = ctl.window(200, 0, 200, 300, false);
    let dir = tempfile::tempdir().unwrap();
    let (paths, expected) = test_files(dir.path());

    ctl.motion(300, 150);
    ctl.button1(true);
    ctl.key(XK_SHIFT_L, true);
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    ctl.motion(100, 100);
    std::thread::sleep(Duration::from_millis(150));
    ctl.motion(105, 110);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(session.feedback(), DragFeedback::Move);
    ctl.button1(false);
    let result = wait_result(&session, Duration::from_secs(5));
    ctl.key(XK_SHIFT_L, false);
    assert_eq!(result.unwrap(), DropResult::Move);

    let log = log.join().unwrap();
    let atoms = &ctl.atoms;
    assert!(!log.positions.is_empty());
    for (action, list) in &log.positions {
        assert_eq!(*action, atoms.move_);
        assert_eq!(list, &vec![atoms.move_]);
    }
    assert_eq!(log.data.as_deref(), Some(expected.as_slice()));
    // The target "moved" nothing and we delete nothing: the files stay.
    assert!(paths.iter().all(|p| p.exists()));
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_shift_let_go_just_before_the_release_copies() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let (_target, log) = spawn_target_finishing(x.name(), (0, 0, 200, 300), None);
    let own = ctl.window(200, 0, 200, 300, false);
    let dir = tempfile::tempdir().unwrap();
    let (paths, expected) = test_files(dir.path());

    ctl.motion(300, 150);
    ctl.button1(true);
    ctl.key(XK_SHIFT_L, true);
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    ctl.motion(100, 100);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(session.feedback(), DragFeedback::Move);
    // Shift up and the button up with no pointer move and no poll in between.
    ctl.key(XK_SHIFT_L, false);
    ctl.button1(false);
    let result = wait_result(&session, Duration::from_secs(5));
    assert_eq!(result.unwrap(), DropResult::Copy);

    let log = log.join().unwrap();
    let atoms = &ctl.atoms;
    // The drop came after a position with the release's action (Copy), at the same spot.
    assert_eq!(log.positions.last().map(|p| p.0), Some(atoms.copy), "{log:?}");
    assert_eq!(log.drops, 1);
    assert_eq!(log.data.as_deref(), Some(expected.as_slice()));
    assert!(paths.iter().all(|p| p.exists()));
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_escape_and_cancel_leave_without_a_drop() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());
    let own = ctl.window(200, 0, 200, 300, false);

    // Escape.
    let (_target, log) = spawn_target(x.name(), (0, 0, 200, 300), Action::Copy);
    ctl.motion(100, 100);
    ctl.button1(true);
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(session.feedback(), DragFeedback::Copy);
    ctl.key(XK_ESCAPE, true);
    let result = wait_result(&session, Duration::from_secs(5));
    ctl.key(XK_ESCAPE, false);
    ctl.button1(false);
    assert_eq!(result.unwrap(), DropResult::None);
    let log = log.join().unwrap();
    assert_eq!(
        (log.enters.len(), log.leaves, log.drops),
        (1, 1, 0),
        "{log:?}"
    );

    // cancel() from the app.
    let (_target, log) = spawn_target(x.name(), (0, 0, 200, 300), Action::Copy);
    ctl.motion(100, 100);
    ctl.button1(true);
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    session.cancel();
    let result = wait_result(&session, Duration::from_secs(5));
    ctl.button1(false);
    assert_eq!(result.unwrap(), DropResult::None);
    let log = log.join().unwrap();
    assert_eq!((log.leaves, log.drops), (1, 0), "{log:?}");

    // Released over nothing: no messages at all, result None.
    ctl.motion(300, 150);
    ctl.button1(true);
    let session = start_drag_out_on(x.name(), own, &items(&paths)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    ctl.button1(false);
    assert_eq!(
        wait_result(&session, Duration::from_secs(5)).unwrap(),
        DropResult::None
    );
    assert_eq!(session.feedback(), DragFeedback::NoTarget);
}

/// A stand-in for winit's toplevel with our target registered, and a wake counter.
struct Registered {
    handle: Option<crate::drop_target::DropTargetHandle>,
    top: Window,
    wakes: Arc<AtomicUsize>,
}

fn register_target(display: &str, ctl: &Ctl) -> Registered {
    let top = ctl.window(0, 0, 300, 300, true);
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let handle = crate::drop_target::register_on(
        display,
        top as isize,
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .unwrap();
    // The proxy: named on the toplevel, naming itself, XDND 5.
    let proxy = ctl.proxy_of(top).expect("XdndProxy on the toplevel");
    assert_eq!(ctl.proxy_of(proxy), Some(proxy));
    assert_eq!(
        property32(&ctl.conn, proxy, ctl.atoms.aware, AtomEnum::ATOM.into()),
        Some(vec![5])
    );
    Registered {
        handle: Some(handle),
        top,
        wakes,
    }
}

/// Runs `drop_files_on` against the registered target while this thread plays the app:
/// takes the events and answers `answer` while dragging. Returns (events, keys seen, result).
fn play_drop(
    display: &str,
    target: &Registered,
    paths: &[PathBuf],
    ctrl: bool,
    shift: bool,
    answer: DropEffect,
) -> (Vec<DropEvent>, Option<(bool, bool)>, DropResult) {
    let handle = target.handle.as_ref().unwrap();
    let (display, top, owned) = (display.to_string(), target.top, paths.to_vec());
    let source =
        std::thread::spawn(move || drop_files_on(&display, top, (40, 50), None, &owned, ctrl, shift));
    let mut events = Vec::new();
    let mut keys = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !source.is_finished() {
        assert!(Instant::now() < deadline, "the drop did not finish");
        events.extend(handle.take_events());
        if handle.is_dragging() {
            keys = Some(handle.keys());
            handle.set_answer(answer);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = source.join().unwrap().unwrap();
    events.extend(handle.take_events());
    (events, keys, result)
}

fn drop_target_case(answer: DropEffect, ctrl: bool, shift: bool, expected: DropResult) {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let mut target = register_target(x.name(), &ctl);
    let dir = tempfile::tempdir().unwrap();
    let (mut paths, _) = test_files(dir.path());
    // A symlink is reported as itself, not as what it points to.
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&paths[0], &link).unwrap();
    paths.push(link);

    let (events, keys, result) = play_drop(x.name(), &target, &paths, ctrl, shift, answer);
    assert_eq!(result, expected);
    assert_eq!(keys, Some((ctrl, shift)));
    assert_eq!(events.len(), 2, "Enter then Drop");
    match &events[0] {
        DropEvent::Enter(info) => assert_eq!(info.files, paths),
        _ => panic!("first event is not Enter"),
    }
    match &events[1] {
        DropEvent::Drop {
            info,
            virtual_source,
            ctrl: drop_ctrl,
            shift: drop_shift,
            answered,
            ..
        } => {
            assert_eq!(info.files, paths);
            assert!(virtual_source.is_none());
            assert_eq!((*drop_ctrl, *drop_shift), (ctrl, shift));
            assert_eq!(*answered, answer);
        }
        _ => panic!("second event is not Drop"),
    }
    let handle = target.handle.as_ref().unwrap();
    assert!(!handle.is_dragging());
    assert!(target.wakes.load(Ordering::SeqCst) >= 2);
    assert!(paths.iter().all(|p| p.exists()));

    // Dropping the handle removes the proxy.
    target.handle = None;
    assert_eq!(ctl.proxy_of(target.top), None);
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drop_target_copy_reports_files_and_finishes_with_copy() {
    drop_target_case(DropEffect::Copy, false, false, DropResult::Copy);
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drop_target_move_finishes_as_private() {
    // `drop_files_on` maps XdndActionPrivate to Move (and a plain Move to Other).
    drop_target_case(DropEffect::Move, false, true, DropResult::Move);
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drop_target_reads_ctrl_from_the_pointer() {
    drop_target_case(DropEffect::Copy, true, false, DropResult::Copy);
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drop_target_ends_the_drag_when_the_source_dies() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let target = register_target(x.name(), &ctl);
    let handle = target.handle.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());

    let source = Source::new(x.name(), paths.clone(), ALL_ACTIONS).unwrap();
    let site = source.site(target.top, 5).unwrap();
    assert_ne!(site.deliver, target.top, "messages go to the proxy");
    source.enter(site);
    source.position(site, 10, 10, Action::Copy);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut source = source;
    while !handle.is_dragging() {
        assert!(Instant::now() < deadline, "no Enter");
        // Serve the data (the target asks for it at the first position).
        while let Some(event) = next_event(
            &source.conn,
            Some(Instant::now() + Duration::from_millis(10)),
        )
        .unwrap()
        {
            source.handle(event);
        }
    }
    let events = handle.take_events();
    assert!(matches!(&events[..], [DropEvent::Enter(info)] if info.files == paths));
    // The source's process "dies": its connection closes and its window goes away.
    drop(source);
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.is_dragging() {
        assert!(Instant::now() < deadline, "the drag did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(matches!(&handle.take_events()[..], [DropEvent::Leave]));
}

// ---- One position in flight (XdndStatus has no sequence number) ----

/// What [`spawn_slow_target`] saw.
#[derive(Debug, Default)]
struct SlowLog {
    positions: usize,
    /// Positions that arrived while an earlier one was still unanswered.
    overlapped: usize,
    drops: usize,
    leaves: usize,
}

/// A mapped XDND target that answers each position (accepting, Copy) `reply` after it came,
/// or never with `None`, and finishes a drop with Copy. Before each answer it looks for
/// positions that came meanwhile: a source with one position in flight sends none.
fn spawn_slow_target(
    display: &str,
    rect: (i16, i16, u16, u16),
    reply: Option<Duration>,
) -> (Window, std::thread::JoinHandle<SlowLog>) {
    let ctl = Ctl::new(display);
    let window = ctl.window(rect.0, rect.1, rect.2, rect.3, true);
    let thread = std::thread::spawn(move || {
        let Ctl { conn, atoms, .. } = ctl;
        let mut log = SlowLog::default();
        let mut queue = std::collections::VecDeque::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let event = match queue.pop_front() {
                Some(event) => event,
                None => match next_event(&conn, Some(deadline)).unwrap() {
                    Some(event) => event,
                    None => return log,
                },
            };
            let Event::ClientMessage(e) = event else {
                continue;
            };
            let l = e.data.as_data32();
            if e.type_ == atoms.position {
                log.positions += 1;
                let Some(delay) = reply else {
                    continue;
                };
                std::thread::sleep(delay);
                while let Some(event) = conn.poll_for_event().unwrap() {
                    if matches!(&event, Event::ClientMessage(m) if m.type_ == atoms.position) {
                        log.overlapped += 1;
                    }
                    queue.push_back(event);
                }
                send_message(&conn, l[0], l[0], atoms.status, [window, 1 | 2, 0, 0, l[4]]);
                conn.flush().unwrap();
            } else if e.type_ == atoms.leave {
                log.leaves += 1;
                return log;
            } else if e.type_ == atoms.drop {
                log.drops += 1;
                send_message(
                    &conn,
                    l[0],
                    l[0],
                    atoms.finished,
                    [window, 1, atoms.action(Action::Copy), 0, 0],
                );
                conn.sync().unwrap();
                return log;
            }
        }
    });
    (window, thread)
}

/// Starts a drag-out with the button held over our own window, then moves over the target.
fn drag_over_target(x: &X, ctl: &Ctl, paths: &[PathBuf]) -> DragOutSession {
    let own = ctl.window(200, 0, 200, 300, false);
    ctl.motion(300, 150);
    ctl.button1(true);
    let session = start_drag_out_on(x.name(), own, &items(paths)).unwrap();
    ctl.motion(100, 100);
    session
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_keeps_one_position_in_flight_to_a_slow_target() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let (_target, log) =
        spawn_slow_target(x.name(), (0, 0, 200, 300), Some(Duration::from_millis(150)));
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());
    let session = drag_over_target(&x, &ctl, &paths);
    // Much faster motion than the target answers: it is coalesced, never queued.
    for step in 0..40 {
        ctl.motion(100 + step, 100 + step);
        std::thread::sleep(Duration::from_millis(20));
    }
    ctl.button1(false);
    let result = wait_result(&session, Duration::from_secs(5));
    assert_eq!(result.unwrap(), DropResult::Copy);
    let log = log.join().unwrap();
    assert_eq!(log.overlapped, 0, "{log:?}");
    assert!(log.positions >= 3, "{log:?}");
    assert_eq!(log.drops, 1, "{log:?}");
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_to_a_target_that_never_answers_cancels_at_the_release() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let (_target, log) = spawn_slow_target(x.name(), (0, 0, 200, 300), None);
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());
    let session = drag_over_target(&x, &ctl, &paths);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(session.feedback(), DragFeedback::Refused);
    ctl.motion(120, 130);
    std::thread::sleep(Duration::from_millis(100));
    ctl.button1(false);
    let result = wait_result(&session, Duration::from_secs(5));
    assert_eq!(result.unwrap(), DropResult::None);
    let log = log.join().unwrap();
    // One position, unanswered; then no more positions, and a leave, never a drop.
    assert_eq!((log.positions, log.drops, log.leaves), (1, 0, 1), "{log:?}");
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drag_out_never_drops_on_a_late_answer() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    // Answers after 700 ms, past the source's 500 ms wait.
    let (_target, log) =
        spawn_slow_target(x.name(), (0, 0, 200, 300), Some(Duration::from_millis(700)));
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());
    let session = drag_over_target(&x, &ctl, &paths);
    // The late (accepting) answer arrives meanwhile.
    std::thread::sleep(Duration::from_millis(1000));
    ctl.motion(120, 130);
    std::thread::sleep(Duration::from_millis(100));
    ctl.button1(false);
    let result = wait_result(&session, Duration::from_secs(5));
    assert_eq!(result.unwrap(), DropResult::None);
    let log = log.join().unwrap();
    assert_eq!((log.positions, log.drops, log.leaves), (1, 0, 1), "{log:?}");
}

#[test]
#[ignore = "starts Xvfb"]
fn xdnd_drop_target_reports_the_last_position_with_the_drop() {
    let x = X::start();
    let ctl = Ctl::new(x.name());
    let target = register_target(x.name(), &ctl);
    let dir = tempfile::tempdir().unwrap();
    let (paths, _) = test_files(dir.path());
    let handle = target.handle.as_ref().unwrap();
    let (display, top, owned) = (x.name().to_string(), target.top, paths.clone());
    // Accepted at (40, 50); then a position at (220, 180) sent together with the drop.
    let source = std::thread::spawn(move || {
        drop_files_on(&display, top, (40, 50), Some((220, 180)), &owned, false, false)
    });
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !source.is_finished() {
        assert!(Instant::now() < deadline, "the drop did not finish");
        events.extend(handle.take_events());
        if handle.is_dragging() {
            handle.set_answer(DropEffect::Copy);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(source.join().unwrap().unwrap(), DropResult::Copy);
    events.extend(handle.take_events());
    let point = events.iter().find_map(|event| match event {
        DropEvent::Drop { point, .. } => Some(*point),
        _ => None,
    });
    assert_eq!(point, Some(Some((220, 180))));
}
