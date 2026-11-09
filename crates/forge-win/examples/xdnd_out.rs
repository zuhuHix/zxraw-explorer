//! Manual check of the Linux XDND code against real file managers, on `$DISPLAY`.
//!
//! * `cargo run -p forge-win --example xdnd_out -- out FILE...` opens a small window titled
//!   "xdnd_out"; pressing the mouse button in it starts a drag-out of the files
//!   (`forge_win::xdnd::start_drag_out`). The feedback and the result are printed.
//! * `cargo run -p forge-win --example xdnd_out -- in [copy|move]` opens a window titled
//!   "xdnd_in" with our drop target registered, answering Copy (or Move), and prints the
//!   events until the first drop.

#[cfg(target_os = "linux")]
fn main() {
    use std::time::{Duration, Instant};

    use forge_win::dragout::DragItem;
    use forge_win::drop_target::{DropEffect, DropEvent};
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;

    let mut args = std::env::args_os().skip(1);
    let mode = args
        .next()
        .map(|mode| mode.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (conn, screen) = x11rb::connect(None).expect("connect to $DISPLAY");
    let screen = &conn.setup().roots[screen];
    let window = conn.generate_id().unwrap();
    let title = if mode == "in" { "xdnd_in" } else { "xdnd_out" };
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0,
        0,
        240,
        160,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .background_pixel(if mode == "in" { 0x2266aa } else { 0xaa6622 })
            .event_mask(EventMask::BUTTON_PRESS | EventMask::EXPOSURE),
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )
    .unwrap();
    // Like winit's window, ours is XdndAware.
    let aware = conn
        .intern_atom(false, b"XdndAware")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    conn.change_property32(PropMode::REPLACE, window, aware, AtomEnum::ATOM, &[5])
        .unwrap();
    conn.map_window(window).unwrap();
    conn.flush().unwrap();
    println!("window {window:#x}");

    if mode == "in" {
        let answer = match args.next().as_deref().and_then(|a| a.to_str()) {
            Some("move") => DropEffect::Move,
            _ => DropEffect::Copy,
        };
        let handle = forge_win::drop_target::register(window as isize, Box::new(|| {})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last_keys = None;
        while Instant::now() < deadline {
            while let Ok(Some(_)) = conn.poll_for_event() {}
            for event in handle.take_events() {
                match event {
                    DropEvent::Enter(info) => println!("Enter {:?}", info.files),
                    DropEvent::Leave => println!("Leave"),
                    DropEvent::Drop {
                        info,
                        ctrl,
                        shift,
                        answered,
                        ..
                    } => {
                        println!(
                            "Drop {:?} ctrl={ctrl} shift={shift} answered={answered:?}",
                            info.files
                        );
                        std::thread::sleep(Duration::from_millis(300));
                        return;
                    }
                }
            }
            if handle.is_dragging() {
                if last_keys != Some(handle.keys()) {
                    last_keys = Some(handle.keys());
                    println!("keys (ctrl, shift) = {:?}", handle.keys());
                }
                handle.set_answer(answer);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        println!("no drop within 120 s");
        return;
    }

    let items: Vec<DragItem> = args.map(|path| DragItem::unknown(path.into())).collect();
    loop {
        match conn.wait_for_event().unwrap() {
            Event::ButtonPress(e) if e.event == window && e.detail == 1 => break,
            _ => {}
        }
    }
    println!("button pressed: starting the drag");
    let session = forge_win::xdnd::start_drag_out(window, &items).unwrap();
    let mut last = None;
    let result = loop {
        while let Ok(Some(_)) = conn.poll_for_event() {}
        let feedback = session.feedback();
        if last != Some(feedback) {
            println!("feedback {feedback:?}");
            last = Some(feedback);
        }
        if let Some(result) = session.poll() {
            break result;
        }
        std::thread::sleep(Duration::from_millis(16));
    };
    println!("result {result:?}");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("xdnd_out runs on Linux (X11) only");
}
