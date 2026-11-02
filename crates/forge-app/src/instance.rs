//! One window per user session: a launch with a path while ZXraw Explorer is running opens
//! that path in a new tab of the running window, and this process exits. The pipe, its
//! security checks and the races are in `forge_win::single_instance`.
//!
//! * A folder opens in a new tab (or the tab already showing it), a file opens its folder
//!   with the file selected, and a photo also opens in the viewer: the same as a cold start
//!   with that path ([`crate::app::ZxrawApp::open_launch_path`]).
//! * A launch without a path only brings the running window to the front. A new tab at the
//!   home folder on every Start-menu click would pile up tabs the user did not ask for.
//! * `--new-window`, the setting "Open folders and files in a new tab of the running window"
//!   turned off, automated runs, and the installer switches (handled before this) always
//!   open their own window.
//! * A menu command (`--extract-here <archives>` and the others in `shell_actions`, or the
//!   Explorer menu's COM activation, which `main` turns into the same [`Launch`]) runs in
//!   the running window, exactly once, with the whole selection; on a cold start it runs in
//!   the new window.
//! * Installed, development and test builds use different pipes (see [`tag`]), so a
//!   development run never hands its paths to the installed copy or takes the installed
//!   copy's launches.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use forge_win::MenuVerb;
use forge_win::single_instance::{self as si, Handoff, Inbox, Startup};

/// Forces a separate window; removed from the arguments before they are read as paths.
pub const NEW_WINDOW: &str = "--new-window";

/// The command line, without the program name and without [`NEW_WINDOW`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub paths: Vec<OsString>,
    pub new_window: bool,
    /// A menu command to run on the paths (`--extract-here` …, the first argument).
    pub action: Option<MenuVerb>,
}

impl Launch {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Self {
        let mut new_window = false;
        let mut paths: Vec<OsString> = args
            .into_iter()
            .filter(|a| {
                let switch = a == NEW_WINDOW;
                new_window |= switch;
                !switch
            })
            .collect();
        let action = paths.first().and_then(|a| crate::shell_actions::verb_of_switch(a));
        if action.is_some() {
            paths.remove(0);
        }
        Self {
            paths,
            new_window,
            action,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::args_os().skip(1))
    }
}

/// This process's launch, set once by `main` (from the command line, or from what the
/// Explorer menu dropped on its COM server).
static STARTUP: OnceLock<Launch> = OnceLock::new();

pub fn set_startup_launch(launch: Launch) {
    let _ = STARTUP.set(launch);
}

/// This process's launch ([`set_startup_launch`], else the command line).
pub fn startup_launch() -> Launch {
    STARTUP.get().cloned().unwrap_or_else(Launch::from_env)
}

/// Menu commands this window runs on its next frame: a cold start's own command waits
/// here until the window is up (a folder picker before the first frame would block it).
static QUEUED: Mutex<Vec<(MenuVerb, Vec<PathBuf>)>> = Mutex::new(Vec::new());

/// What a cold start does with its own launch: queues a menu command for the first frame,
/// or opens the first path at once.
pub fn open_startup_launch(app: &mut crate::app::ZxrawApp, ctx: &egui::Context) {
    let launch = startup_launch();
    match launch.action {
        Some(verb) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let paths = launch.paths.iter().map(|p| cwd.join(p)).collect();
            QUEUED
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((verb, paths));
            ctx.request_repaint();
        }
        None => {
            if let Some(arg) = launch.paths.first() {
                app.open_launch_path(ctx, arg);
            }
        }
    }
}

/// What `main` does after [`startup`].
pub enum Decision {
    /// Exit with this code: the running window took the launch (0), or forwarding was
    /// required (`ZXRAW_FORWARD_ONLY`, tests) and failed (3).
    Exit(i32),
    /// Open a window. Keep `server` alive until exit; hand `inbox` to the app.
    Run {
        inbox: Option<Arc<Inbox>>,
        server: Option<si::Server>,
    },
}

/// The pipe tag: `ZXRAW_INSTANCE_NAME` in debug and automation builds (tests use a unique
/// one), otherwise `instance` for release builds and `instance-dev` for debug builds.
/// `None`: this process takes no part (an automated run without its own name).
fn tag(automated: bool) -> Option<String> {
    #[cfg(any(debug_assertions, feature = "automation"))]
    if let Some(name) = std::env::var_os("ZXRAW_INSTANCE_NAME") {
        return Some(name.to_string_lossy().into_owned());
    }
    if automated {
        return None;
    }
    Some(if cfg!(debug_assertions) { "instance-dev" } else { "instance" }.to_string())
}

/// Tests: fail instead of opening a window when the launch could not be handed over, so a
/// test never opens a window with a real profile.
fn forward_only() -> bool {
    cfg!(any(debug_assertions, feature = "automation"))
        && std::env::var_os("ZXRAW_FORWARD_ONLY").is_some()
}

/// Hands the launch to the running window, or prepares this process to be the window that
/// receives later launches.
pub fn startup(launch: &Launch, settings: &crate::settings::Settings) -> Decision {
    let none = Decision::Run { inbox: None, server: None };
    let automated = crate::testhook::env_requests_automation();
    let Some(tag) = tag(automated) else {
        return none;
    };
    if !settings.open_in_running_window && !forward_only() {
        return none;
    }
    let channel = match si::Channel::current(&tag) {
        Ok(channel) => channel,
        Err(e) => {
            tracing::warn!("single-window launches are off: {e}");
            return if forward_only() { Decision::Exit(3) } else { none };
        }
    };
    let inbox = Arc::new(Inbox::new());
    if launch.new_window {
        // Its own window, but it still receives later launches when no other window does.
        let server = match si::Server::claim(&channel, inbox.clone()) {
            Ok(server) => server,
            Err(e) => {
                tracing::warn!("could not create the instance pipe: {e}");
                None
            }
        };
        return Decision::Run { inbox: Some(inbox), server };
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths: Vec<PathBuf> = launch.paths.iter().map(PathBuf::from).collect();
    // A command without items has nothing to run: it only brings the window forward.
    let action = launch.action.filter(|_| !paths.is_empty()).map(MenuVerb::name);
    let request = match si::encode_action_request(&cwd, &paths, action) {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!("this launch cannot be handed to a running window ({e})");
            return if forward_only() { Decision::Exit(3) } else { none };
        }
    };
    match si::claim_or_forward(&channel, inbox.clone(), &request, si::FORWARD_TIMEOUT) {
        Startup::Primary(server) => Decision::Run {
            inbox: Some(inbox),
            server: Some(server),
        },
        Startup::Forwarded => {
            tracing::info!("opened in the running window");
            Decision::Exit(0)
        }
        Startup::Standalone(why) => {
            tracing::warn!("opening a new window: {why}");
            if forward_only() {
                return Decision::Exit(3);
            }
            // The other window may be closing: take over its role once it is gone.
            si::claim_later(channel, inbox.clone(), std::time::Duration::from_secs(120));
            Decision::Run { inbox: Some(inbox), server: None }
        }
    }
}

/// Connects the inbox to the new window: a request repaints it (so an idle window takes it
/// at once) and brings it to the front. A hidden automated window is never brought forward.
pub fn attach(cc: &eframe::CreationContext<'_>, inbox: &Inbox, hidden: bool) {
    let ctx = cc.egui_ctx.clone();
    inbox.set_waker(move || ctx.request_repaint());
    if hidden {
        return;
    }
    use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    if let Ok(RawWindowHandle::Win32(handle)) = cc.window_handle().map(|h| h.as_raw()) {
        inbox.set_window(handle.hwnd.get());
    }
}

/// Runs this window's queued menu commands and opens or runs the launches other processes
/// handed over since the last frame.
pub fn tick(app: &mut crate::app::ZxrawApp, ctx: &egui::Context) {
    let queued = std::mem::take(&mut *QUEUED.lock().unwrap_or_else(|p| p.into_inner()));
    for (verb, paths) in queued {
        crate::shell_actions::run(app, ctx, verb, paths);
    }
    let Some(inbox) = app.instance.clone() else {
        return;
    };
    for Handoff { action, paths } in inbox.take_handoffs() {
        match action.as_deref().map(MenuVerb::from_name) {
            None => {
                for path in paths {
                    app.open_launch_path(ctx, path.as_os_str());
                }
            }
            Some(Some(verb)) => crate::shell_actions::run(app, ctx, verb, paths),
            Some(None) => tracing::warn!("ignored an unknown action {action:?}"),
        }
    }
}

/// The window is closing: later launches open their own window.
pub fn close(app: &crate::app::ZxrawApp) {
    if let Some(inbox) = &app.instance {
        inbox.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_window_switch_is_removed_from_the_paths() {
        let launch = Launch::parse(["--new-window".into(), r"C:\Photos".into()]);
        assert!(launch.new_window);
        assert_eq!(launch.paths, vec![OsString::from(r"C:\Photos")]);
        let launch = Launch::parse([r"C:\Photos".into(), "x".into()]);
        assert!(!launch.new_window);
        assert_eq!(launch.paths.len(), 2);
        assert_eq!(launch.action, None);
    }

    #[test]
    fn a_leading_menu_switch_is_the_action_and_the_rest_are_its_items() {
        let launch = Launch::parse([
            "--add-zip".into(),
            r"C:\a.jpg".into(),
            "--new-window".into(),
            r"C:\b".into(),
        ]);
        assert_eq!(launch.action, Some(MenuVerb::AddZip));
        assert!(launch.new_window);
        assert_eq!(launch.paths, [OsString::from(r"C:\a.jpg"), OsString::from(r"C:\b")]);
        // Only the first argument names an action: a later one is a (strange) path.
        let launch = Launch::parse([r"C:\a.zip".into(), "--extract-here".into()]);
        assert_eq!(launch.action, None);
        assert_eq!(launch.paths.len(), 2);
    }


    #[test]
    fn automated_runs_take_part_only_with_their_own_name() {
        // The test binary has no ZXRAW_INSTANCE_NAME.
        if std::env::var_os("ZXRAW_INSTANCE_NAME").is_none() {
            assert_eq!(tag(true), None);
            assert_eq!(tag(false).as_deref(), Some("instance-dev"));
        }
    }
}
