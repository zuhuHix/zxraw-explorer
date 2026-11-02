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
//! * Installed, development and test builds use different pipes (see [`tag`]), so a
//!   development run never hands its paths to the installed copy or takes the installed
//!   copy's launches.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use forge_win::single_instance::{self as si, Inbox, Startup};

/// Forces a separate window; removed from the arguments before they are read as paths.
pub const NEW_WINDOW: &str = "--new-window";

/// The command line, without the program name and without [`NEW_WINDOW`].
pub struct Launch {
    pub paths: Vec<OsString>,
    pub new_window: bool,
}

impl Launch {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Self {
        let mut new_window = false;
        let paths = args
            .into_iter()
            .filter(|a| {
                let switch = a == NEW_WINDOW;
                new_window |= switch;
                !switch
            })
            .collect();
        Self { paths, new_window }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::args_os().skip(1))
    }
}

/// The path a cold start opens: the first one on the command line.
pub fn launch_path() -> Option<OsString> {
    Launch::from_env().paths.into_iter().next()
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
    let request = match si::encode_request(&cwd, &paths) {
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

/// Opens the launches other processes handed over since the last frame.
pub fn tick(app: &mut crate::app::ZxrawApp, ctx: &egui::Context) {
    let Some(inbox) = app.instance.clone() else {
        return;
    };
    for paths in inbox.take() {
        for path in paths {
            app.open_launch_path(ctx, path.as_os_str());
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
