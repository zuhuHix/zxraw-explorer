//! Every user action is a [`Cmd`]: keyboard shortcuts, menus, toolbar buttons and the
//! command palette all dispatch through the same enum so they stay consistent.

use std::collections::BTreeMap;

use egui::{Key, KeyboardShortcut, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cmd {
    Back,
    Forward,
    Up,
    Refresh,
    Open,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    ToggleDualPane,
    SwitchPane,
    FocusAddress,
    FocusFilter,
    SelectAll,
    SelectNone,
    InvertSelection,
    Copy,
    Cut,
    Paste,
    CopyToOtherPane,
    MoveToOtherPane,
    Recycle,
    DeletePermanently,
    Rename,
    BatchRename,
    NewFolder,
    NewFile,
    CopyPath,
    Properties,
    OpenWith,
    RevealInExplorer,
    ToggleHidden,
    TogglePreview,
    ToggleSidebar,
    ViewDetails,
    ViewGrid,
    ThumbsBigger,
    ThumbsSmaller,
    CommandPalette,
    BookmarkFolder,
    ShowJobs,
    Settings,
    Search,
    FolderSize,
    FindDuplicates,
    ChecksumFiles,
    CompareFolders,
    CullMode,
    CompareImages,
    Ingest,
    ToggleTheme,
    Undo,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Rate0,
    Rate1,
    Rate2,
    Rate3,
    Rate4,
    Rate5,
    LabelRed,
    LabelYellow,
    LabelGreen,
    LabelBlue,
    LabelPurple,
    LabelNone,
    MarkPick,
    MarkReject,
    Unflag,
    IndexFolder,
    OpenIn,
    Collections,
    AddToCollection,
    GenerateProxies,
    ContactSheet,
    BrowseArchive,
    CreateArchive,
    SplitFile,
    JoinFiles,
    GroupSimilar,
    ExportMetadata,
    LocateProxies,
    SelectOriginals,
    SelectExports,
    BatchExport,
    Deliver,
    EditMetadata,
    SyncFolders,
    FindReplace,
    VerifyBackup,
    DownloadFfmpeg,
    CheckForUpdates,
    PinToQuickAccess,
    SaveLayout,
    LoadLayout,
    ManageLayouts,
    ToggleStickySelection,
}

impl Cmd {
    pub const ALL: &'static [Cmd] = &[
        Cmd::Back,
        Cmd::Forward,
        Cmd::Up,
        Cmd::Refresh,
        Cmd::Open,
        Cmd::NewTab,
        Cmd::CloseTab,
        Cmd::NextTab,
        Cmd::PrevTab,
        Cmd::ToggleDualPane,
        Cmd::SwitchPane,
        Cmd::FocusAddress,
        Cmd::FocusFilter,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::InvertSelection,
        Cmd::Copy,
        Cmd::Cut,
        Cmd::Paste,
        Cmd::CopyToOtherPane,
        Cmd::MoveToOtherPane,
        Cmd::Recycle,
        Cmd::DeletePermanently,
        Cmd::Rename,
        Cmd::BatchRename,
        Cmd::NewFolder,
        Cmd::NewFile,
        Cmd::CopyPath,
        Cmd::Properties,
        Cmd::OpenWith,
        Cmd::RevealInExplorer,
        Cmd::ToggleHidden,
        Cmd::TogglePreview,
        Cmd::ToggleSidebar,
        Cmd::ViewDetails,
        Cmd::ViewGrid,
        Cmd::ThumbsBigger,
        Cmd::ThumbsSmaller,
        Cmd::CommandPalette,
        Cmd::BookmarkFolder,
        Cmd::ShowJobs,
        Cmd::Settings,
        Cmd::Search,
        Cmd::FolderSize,
        Cmd::FindDuplicates,
        Cmd::ChecksumFiles,
        Cmd::CompareFolders,
        Cmd::CullMode,
        Cmd::CompareImages,
        Cmd::Ingest,
        Cmd::ToggleTheme,
        Cmd::Undo,
        Cmd::ZoomIn,
        Cmd::ZoomOut,
        Cmd::ZoomReset,
        Cmd::Rate0,
        Cmd::Rate1,
        Cmd::Rate2,
        Cmd::Rate3,
        Cmd::Rate4,
        Cmd::Rate5,
        Cmd::LabelRed,
        Cmd::LabelYellow,
        Cmd::LabelGreen,
        Cmd::LabelBlue,
        Cmd::LabelPurple,
        Cmd::LabelNone,
        Cmd::MarkPick,
        Cmd::MarkReject,
        Cmd::Unflag,
        Cmd::IndexFolder,
        Cmd::OpenIn,
        Cmd::Collections,
        Cmd::AddToCollection,
        Cmd::GenerateProxies,
        Cmd::ContactSheet,
        Cmd::BrowseArchive,
        Cmd::CreateArchive,
        Cmd::SplitFile,
        Cmd::JoinFiles,
        Cmd::GroupSimilar,
        Cmd::ExportMetadata,
        Cmd::LocateProxies,
        Cmd::SelectOriginals,
        Cmd::SelectExports,
        Cmd::BatchExport,
        Cmd::Deliver,
        Cmd::EditMetadata,
        Cmd::SyncFolders,
        Cmd::FindReplace,
        Cmd::VerifyBackup,
        Cmd::DownloadFfmpeg,
        Cmd::CheckForUpdates,
        Cmd::PinToQuickAccess,
        Cmd::SaveLayout,
        Cmd::LoadLayout,
        Cmd::ManageLayouts,
        Cmd::ToggleStickySelection,
    ];

    /// Stable identifier used as the key in `Settings::keybindings`. Never rename one: it's
    /// what saved user shortcuts refer to.
    pub fn id(self) -> &'static str {
        match self {
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::Up => "Up",
            Self::Refresh => "Refresh",
            Self::Open => "Open",
            Self::NewTab => "NewTab",
            Self::CloseTab => "CloseTab",
            Self::NextTab => "NextTab",
            Self::PrevTab => "PrevTab",
            Self::ToggleDualPane => "ToggleDualPane",
            Self::SwitchPane => "SwitchPane",
            Self::FocusAddress => "FocusAddress",
            Self::FocusFilter => "FocusFilter",
            Self::SelectAll => "SelectAll",
            Self::SelectNone => "SelectNone",
            Self::InvertSelection => "InvertSelection",
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::CopyToOtherPane => "CopyToOtherPane",
            Self::MoveToOtherPane => "MoveToOtherPane",
            Self::Recycle => "Recycle",
            Self::DeletePermanently => "DeletePermanently",
            Self::Rename => "Rename",
            Self::BatchRename => "BatchRename",
            Self::NewFolder => "NewFolder",
            Self::NewFile => "NewFile",
            Self::CopyPath => "CopyPath",
            Self::Properties => "Properties",
            Self::OpenWith => "OpenWith",
            Self::RevealInExplorer => "RevealInExplorer",
            Self::ToggleHidden => "ToggleHidden",
            Self::TogglePreview => "TogglePreview",
            Self::ToggleSidebar => "ToggleSidebar",
            Self::ViewDetails => "ViewDetails",
            Self::ViewGrid => "ViewGrid",
            Self::ThumbsBigger => "ThumbsBigger",
            Self::ThumbsSmaller => "ThumbsSmaller",
            Self::CommandPalette => "CommandPalette",
            Self::BookmarkFolder => "BookmarkFolder",
            Self::ShowJobs => "ShowJobs",
            Self::Settings => "Settings",
            Self::Search => "Search",
            Self::FolderSize => "FolderSize",
            Self::FindDuplicates => "FindDuplicates",
            Self::ChecksumFiles => "ChecksumFiles",
            Self::CompareFolders => "CompareFolders",
            Self::CullMode => "CullMode",
            Self::CompareImages => "CompareImages",
            Self::Ingest => "Ingest",
            Self::ToggleTheme => "ToggleTheme",
            Self::Undo => "Undo",
            Self::ZoomIn => "ZoomIn",
            Self::ZoomOut => "ZoomOut",
            Self::ZoomReset => "ZoomReset",
            Self::Rate0 => "Rate0",
            Self::Rate1 => "Rate1",
            Self::Rate2 => "Rate2",
            Self::Rate3 => "Rate3",
            Self::Rate4 => "Rate4",
            Self::Rate5 => "Rate5",
            Self::LabelRed => "LabelRed",
            Self::LabelYellow => "LabelYellow",
            Self::LabelGreen => "LabelGreen",
            Self::LabelBlue => "LabelBlue",
            Self::LabelPurple => "LabelPurple",
            Self::LabelNone => "LabelNone",
            Self::MarkPick => "MarkPick",
            Self::MarkReject => "MarkReject",
            Self::Unflag => "Unflag",
            Self::IndexFolder => "IndexFolder",
            Self::OpenIn => "OpenIn",
            Self::Collections => "Collections",
            Self::AddToCollection => "AddToCollection",
            Self::GenerateProxies => "GenerateProxies",
            Self::ContactSheet => "ContactSheet",
            Self::BrowseArchive => "BrowseArchive",
            Self::CreateArchive => "CreateArchive",
            Self::SplitFile => "SplitFile",
            Self::JoinFiles => "JoinFiles",
            Self::GroupSimilar => "GroupSimilar",
            Self::ExportMetadata => "ExportMetadata",
            Self::LocateProxies => "LocateProxies",
            Self::SelectOriginals => "SelectOriginals",
            Self::SelectExports => "SelectExports",
            Self::BatchExport => "BatchExport",
            Self::Deliver => "Deliver",
            Self::EditMetadata => "EditMetadata",
            Self::SyncFolders => "SyncFolders",
            Self::FindReplace => "FindReplace",
            Self::VerifyBackup => "VerifyBackup",
            Self::DownloadFfmpeg => "DownloadFfmpeg",
            Self::CheckForUpdates => "CheckForUpdates",
            Self::PinToQuickAccess => "PinToQuickAccess",
            Self::SaveLayout => "SaveLayout",
            Self::LoadLayout => "LoadLayout",
            Self::ManageLayouts => "ManageLayouts",
            Self::ToggleStickySelection => "ToggleStickySelection",
        }
    }

    pub fn label(self) -> &'static str {
        use Cmd::*;
        match self {
            Back => "Back",
            Forward => "Forward",
            Up => "Up one level",
            Refresh => "Refresh",
            Open => "Open",
            NewTab => "New tab",
            CloseTab => "Close tab",
            NextTab => "Next tab",
            PrevTab => "Previous tab",
            ToggleDualPane => "Toggle dual pane",
            SwitchPane => "Switch to other pane",
            FocusAddress => "Edit address",
            FocusFilter => "Filter current folder",
            SelectAll => "Select all",
            SelectNone => "Select none",
            InvertSelection => "Invert selection",
            Copy => "Copy",
            Cut => "Cut",
            Paste => "Paste",
            CopyToOtherPane => "Copy to other pane",
            MoveToOtherPane => "Move to other pane",
            Recycle => "Delete (to Recycle Bin)",
            DeletePermanently => "Delete permanently…",
            Rename => "Rename",
            BatchRename => "Batch rename…",
            NewFolder => "New folder",
            NewFile => "New empty file",
            CopyPath => "Copy path",
            Properties => "Properties",
            OpenWith => "Open with…",
            RevealInExplorer => "Show in File Explorer",
            ToggleHidden => "Show/hide hidden files",
            TogglePreview => "Toggle preview panel",
            ToggleSidebar => "Toggle sidebar",
            ViewDetails => "Details view",
            ViewGrid => "Thumbnail view",
            ThumbsBigger => "Larger thumbnails",
            ThumbsSmaller => "Smaller thumbnails",
            CommandPalette => "Command palette",
            BookmarkFolder => "Pin this folder to Quick access",
            ShowJobs => "Show operations",
            Settings => "Settings",
            Search => "Search index…",
            FolderSize => "Analyze folder size…",
            FindDuplicates => "Find duplicates…",
            ChecksumFiles => "Compute checksums…",
            CompareFolders => "Compare with other pane…",
            CullMode => "Culling mode",
            CompareImages => "Compare selected images",
            Ingest => "Import media…",
            ToggleTheme => "Toggle light/dark theme",
            Undo => "Undo last operation",
            ZoomIn => "Zoom UI in",
            ZoomOut => "Zoom UI out",
            ZoomReset => "Reset UI zoom",
            Rate0 => "Clear rating",
            Rate1 => "Rate ★",
            Rate2 => "Rate ★★",
            Rate3 => "Rate ★★★",
            Rate4 => "Rate ★★★★",
            Rate5 => "Rate ★★★★★",
            LabelRed => "Label red",
            LabelYellow => "Label yellow",
            LabelGreen => "Label green",
            LabelBlue => "Label blue",
            LabelPurple => "Label purple",
            LabelNone => "Clear color label",
            MarkPick => "Mark as pick",
            MarkReject => "Mark as reject",
            Unflag => "Clear pick/reject",
            IndexFolder => "Index this folder for search",
            OpenIn => "Open in external app…",
            Collections => "Collections & saved searches",
            AddToCollection => "Add to collection…",
            GenerateProxies => "Generate video proxies…",
            ContactSheet => "Create contact sheet…",
            BrowseArchive => "Browse archive",
            CreateArchive => "Create zip archive…",
            SplitFile => "Split file into parts…",
            JoinFiles => "Join split parts…",
            GroupSimilar => "Group bursts & similar photos",
            ExportMetadata => "Export metadata report…",
            LocateProxies => "Locate video proxies…",
            SelectOriginals => "Select originals of exports",
            SelectExports => "Select exports of originals",
            BatchExport => "Export / convert images…",
            Deliver => "Deliver to client…",
            EditMetadata => "Edit metadata (XMP)…",
            SyncFolders => "Sync folders…",
            FindReplace => "Find and replace in files…",
            VerifyBackup => "Verify backup (compare by content)…",
            DownloadFfmpeg => "Download FFmpeg (video tools)…",
            CheckForUpdates => "Check for updates",
            PinToQuickAccess => "Pin to Quick access",
            SaveLayout => "Save layout…",
            LoadLayout => "Load layout…",
            ManageLayouts => "Manage layouts…",
            ToggleStickySelection => "Sticky selection (click adds or removes)",
        }
    }

    pub fn shortcut(self) -> Option<KeyboardShortcut> {
        use Cmd::*;
        let c = Modifiers::COMMAND;
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let a = Modifiers::ALT;
        let s = Modifiers::SHIFT;
        let n = Modifiers::NONE;
        let k = |m, key| Some(KeyboardShortcut::new(m, key));
        match self {
            Back => k(a, Key::ArrowLeft),
            Forward => k(a, Key::ArrowRight),
            Up => k(a, Key::ArrowUp),
            Refresh => k(n, Key::F5),
            NewTab => k(c, Key::T),
            CloseTab => k(c, Key::W),
            NextTab => k(c, Key::Tab),
            PrevTab => k(cs, Key::Tab),
            ToggleDualPane => k(n, Key::F3),
            SwitchPane => k(n, Key::Tab),
            FocusAddress => k(c, Key::L),
            FocusFilter => k(c, Key::F),
            SelectAll => k(c, Key::A),
            SelectNone => k(cs, Key::A),
            InvertSelection => k(c, Key::I),
            Copy => k(c, Key::C),
            Cut => k(c, Key::X),
            Paste => k(c, Key::V),
            CopyToOtherPane => k(n, Key::F6),
            MoveToOtherPane => k(s, Key::F6),
            Recycle => k(n, Key::Delete),
            DeletePermanently => k(s, Key::Delete),
            Rename => k(n, Key::F2),
            BatchRename => k(cs, Key::R),
            NewFolder => k(cs, Key::N),
            CopyPath => k(cs, Key::C),
            Properties => k(a, Key::Enter),
            TogglePreview => k(a, Key::P),
            ToggleHidden => k(c, Key::H),
            ViewDetails => k(cs, Key::Num1),
            ViewGrid => k(cs, Key::Num2),
            CommandPalette => k(cs, Key::P),
            BookmarkFolder => k(c, Key::D),
            ShowJobs => k(cs, Key::J),
            Settings => k(c, Key::Comma),
            Search => k(cs, Key::F),
            CullMode => k(cs, Key::K),
            Ingest => k(cs, Key::I),
            Deliver => k(cs, Key::E),
            Undo => k(c, Key::Z),
            ZoomIn => k(c, Key::Plus),
            ZoomOut => k(c, Key::Minus),
            ZoomReset => k(cs, Key::Num0),
            Rate0 => k(c, Key::Num0),
            Rate1 => k(c, Key::Num1),
            Rate2 => k(c, Key::Num2),
            Rate3 => k(c, Key::Num3),
            Rate4 => k(c, Key::Num4),
            Rate5 => k(c, Key::Num5),
            LabelRed => k(c, Key::Num6),
            LabelYellow => k(c, Key::Num7),
            LabelGreen => k(c, Key::Num8),
            LabelBlue => k(c, Key::Num9),
            MarkPick => k(c, Key::P),
            MarkReject => k(c, Key::Delete),
            Unflag => k(c, Key::U),
            ToggleStickySelection => k(cs, Key::S),
            _ => None,
        }
    }

    /// The shortcut in effect for this command: the user's override from settings if there
    /// is one (`None` = unbound), otherwise the built-in default from [`Cmd::shortcut`].
    /// An override that does not parse is ignored and the default applies.
    pub fn binding(self, overrides: &Keybindings) -> Option<KeyboardShortcut> {
        match overrides.get(self.id()) {
            None => self.shortcut(),
            Some(None) => None,
            Some(Some(text)) if text.trim().is_empty() => None,
            Some(Some(text)) => parse_shortcut(text).ok().or_else(|| self.shortcut()),
        }
    }

    /// Display text of [`Cmd::binding`], empty when unbound.
    pub fn binding_text(self, overrides: &Keybindings) -> String {
        self.binding(overrides).map(|s| shortcut_text(&s)).unwrap_or_default()
    }

    /// Another command whose effective binding is already `text` (a canonical shortcut
    /// string, see [`shortcut_text`]), if any.
    pub fn conflicting(self, overrides: &Keybindings, text: &str) -> Option<Cmd> {
        Cmd::ALL
            .iter()
            .copied()
            .filter(|c| *c != self)
            .find(|c| c.binding_text(overrides) == text)
    }
}

/// User shortcut overrides keyed by [`Cmd::id`]. `None` (JSON `null`) means unbound; a
/// missing key means the built-in default applies.
pub type Keybindings = BTreeMap<String, Option<String>>;

/// What the key handlers need each frame, published by the app's `logic` pass.
#[derive(Clone, Default)]
pub struct KeyState {
    pub overrides: Keybindings,
    /// The settings page is recording a new shortcut: no command may run.
    pub capturing: bool,
}

fn key_state_id() -> egui::Id {
    egui::Id::new("zxraw-key-state")
}

pub fn publish_key_state(ctx: &egui::Context, overrides: &Keybindings, capturing: bool) {
    let state = KeyState { overrides: overrides.clone(), capturing };
    ctx.data_mut(|d| d.insert_temp(key_state_id(), state));
}

/// The state published this frame (default before the first publish).
pub fn published_key_state(ctx: &egui::Context) -> KeyState {
    ctx.data(|d| d.get_temp::<KeyState>(key_state_id())).unwrap_or_default()
}

/// A tooltip such as `Back (Alt+Left)`, or just the label when the command is unbound.
pub fn tooltip(label: &str, cmd: Cmd, overrides: &Keybindings) -> String {
    let text = cmd.binding_text(overrides);
    if text.is_empty() { label.to_string() } else { format!("{label} ({text})") }
}

/// Sets `cmd` to `text` (canonical, see [`shortcut_text`]) or unbinds it when `None`.
pub fn set_binding(overrides: &mut Keybindings, cmd: Cmd, text: Option<String>) {
    overrides.insert(cmd.id().to_string(), text);
}

/// Canonical text for a shortcut: modifiers in the order Ctrl, Alt, Shift, then the key
/// name, e.g. `Ctrl+Shift+P`, `Alt+Left`, `F5`. Parsed back by [`parse_shortcut`].
pub fn shortcut_text(sc: &KeyboardShortcut) -> String {
    let m = sc.modifiers;
    let mut out = String::new();
    if m.command || m.ctrl {
        out.push_str("Ctrl+");
    }
    if m.alt {
        out.push_str("Alt+");
    }
    if m.shift {
        out.push_str("Shift+");
    }
    out.push_str(sc.logical_key.name());
    out
}

/// Parses text such as `Ctrl+Shift+P`, `alt+left` or `F5` (modifier and key names are
/// case-insensitive; `Ctrl`/`Control`, `Alt`, `Shift` are the modifiers). Exactly one key
/// is required and each modifier may appear at most once.
pub fn parse_shortcut(text: &str) -> Result<KeyboardShortcut, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("empty shortcut".into());
    }
    // "+" is itself a key, so "Ctrl++" and "+" need special handling before splitting. The
    // separator before a trailing "+" key is the last '+' of "++", so anything left over
    // ("Ctrl+++") is a surplus delimiter.
    let (head, key_tok) = if t == "+" {
        (None, "+")
    } else if let Some(h) = t.strip_suffix("++") {
        if h.ends_with('+') {
            return Err(format!("surplus `+` in `{text}`"));
        }
        (Some(h), "+")
    } else {
        match t.rsplit_once('+') {
            Some((h, k)) => (Some(h), k.trim()),
            None => (None, t),
        }
    };
    let key = Key::ALL
        .iter()
        .copied()
        .find(|k| k.name().eq_ignore_ascii_case(key_tok))
        .or_else(|| Key::from_name(key_tok))
        .ok_or_else(|| format!("unknown key `{key_tok}` in `{text}`"))?;
    let mut mods = Modifiers::NONE;
    let mut seen = [false; 3];
    if let Some(head) = head {
        for part in head.split('+') {
            let (idx, flag, name) = match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => (0, Modifiers::COMMAND, "Ctrl"),
                "alt" => (1, Modifiers::ALT, "Alt"),
                "shift" => (2, Modifiers::SHIFT, "Shift"),
                "" => return Err(format!("empty modifier in `{text}`")),
                other => return Err(format!("unknown modifier `{other}` in `{text}`")),
            };
            if seen[idx] {
                return Err(format!("`{name}` given twice in `{text}`"));
            }
            seen[idx] = true;
            mods |= flag;
        }
    }
    Ok(KeyboardShortcut::new(mods, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shortcuts_round_trip_through_text() {
        for c in Cmd::ALL {
            if let Some(sc) = c.shortcut() {
                let text = shortcut_text(&sc);
                let back = parse_shortcut(&text).unwrap_or_else(|e| panic!("{c:?}: {e}"));
                assert_eq!(back, sc, "{c:?} via `{text}`");
                assert_eq!(shortcut_text(&back), text);
            }
        }
    }

    #[test]
    fn parses_human_forms() {
        let p = |s: &str| parse_shortcut(s).unwrap();
        assert_eq!(p("Ctrl+Shift+P"), KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::P));
        assert_eq!(p("ctrl+shift+p"), p("Ctrl+Shift+P"));
        assert_eq!(p("Control+T"), KeyboardShortcut::new(Modifiers::COMMAND, Key::T));
        assert_eq!(p("Alt+Left"), KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft));
        assert_eq!(p("F5"), KeyboardShortcut::new(Modifiers::NONE, Key::F5));
        assert_eq!(p("Ctrl+Plus"), KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus));
        assert_eq!(p("Ctrl++"), KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus));
        assert_eq!(p("Ctrl+Shift++"), KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Plus));
        assert_eq!(p("+"), KeyboardShortcut::new(Modifiers::NONE, Key::Plus));
        assert_eq!(p("Ctrl+1"), KeyboardShortcut::new(Modifiers::COMMAND, Key::Num1));
        assert_eq!(p("  Ctrl + Shift + Delete "), KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Delete));
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "", "   ", "Ctrl+", "Ctrl+Shift", "Ctrl+Nope", "Hyper+P", "Ctrl+Ctrl+P", "Ctrl++Shift+P", "Ctrl+ +P",
            "++", "+++", "Ctrl+++", "Ctrl+Shift+++", "Ctrl+Plus+",
        ] {
            assert!(parse_shortcut(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn override_and_default_lookup() {
        let mut map = Keybindings::new();
        assert_eq!(Cmd::NewTab.binding(&map), Cmd::NewTab.shortcut());
        set_binding(&mut map, Cmd::NewTab, Some("Ctrl+Shift+T".into()));
        assert_eq!(Cmd::NewTab.binding_text(&map), "Ctrl+Shift+T");
        set_binding(&mut map, Cmd::CloseTab, None);
        assert_eq!(Cmd::CloseTab.binding(&map), None);
        assert_eq!(Cmd::CloseTab.binding_text(&map), "");
        map.insert(Cmd::Refresh.id().into(), Some("   ".into()));
        assert_eq!(Cmd::Refresh.binding(&map), None);
        map.insert(Cmd::Refresh.id().into(), Some("garbage+".into()));
        assert_eq!(Cmd::Refresh.binding(&map), Cmd::Refresh.shortcut(), "invalid override falls back to default");
        assert_eq!(Cmd::Copy.conflicting(&map, "Ctrl+Shift+T"), Some(Cmd::NewTab));
        assert_eq!(Cmd::Copy.conflicting(&map, "Ctrl+Shift+Y"), None);
    }

    #[test]
    fn ids_are_unique_and_keybindings_load_from_json() {
        let ids: std::collections::HashSet<_> = Cmd::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(ids.len(), Cmd::ALL.len());
        let map: Keybindings = serde_json::from_str(r#"{"NewTab":"Ctrl+Shift+T","CloseTab":null}"#).unwrap();
        assert_eq!(map.get("CloseTab"), Some(&None));
        assert_eq!(map.get("NewTab"), Some(&Some("Ctrl+Shift+T".to_string())));
    }
}
