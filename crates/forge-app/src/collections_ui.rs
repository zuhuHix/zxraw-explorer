//! Collections, saved searches, portable backup, and relinking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::RichText;
use forge_core::model::EntryKind;
use forge_db::{
    Collection, CollectionId, CollectionKind, Db, FileRecord, ImportReport, NewFile, RelinkReport,
    SavedSearch, SearchOptions, SearchScope, SmartCollectionConflict,
    SmartCollectionConflictChoice,
};

use crate::app::ZxrawApp;

const SEARCH_RESULT_LIMIT: u32 = 5_000;

pub struct CollectionsUi {
    pub open: bool,
    tx: Sender<UiMessage>,
    rx: Receiver<UiMessage>,
    collections: Vec<Collection>,
    saved_searches: Vec<SavedSearch>,
    catalog_loaded: bool,
    catalog_loading: bool,
    detail: Option<Detail>,
    detail_requested: bool,
    rows: Vec<FileRecord>,
    rows_loading: bool,
    rows_error: Option<String>,
    operation_pending: bool,
    feedback: Option<(String, bool)>,
    picker_open: bool,
    picker_filter: String,
    picker_files: Vec<NewFile>,
    edit: Option<EditDialog>,
    delete_confirm: Option<DeleteTarget>,
    relink_open: bool,
    relink_old: Option<PathBuf>,
    relink_new: Option<PathBuf>,
    relink_preview_count: Option<u64>,
    relink_preview_running: bool,
    relink_preview_error: Option<String>,
    relink_confirm: bool,
    import_prompt: Option<ImportPrompt>,
}

impl Default for CollectionsUi {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            open: false,
            tx,
            rx,
            collections: Vec::new(),
            saved_searches: Vec::new(),
            catalog_loaded: false,
            catalog_loading: false,
            detail: None,
            detail_requested: false,
            rows: Vec::new(),
            rows_loading: false,
            rows_error: None,
            operation_pending: false,
            feedback: None,
            picker_open: false,
            picker_filter: String::new(),
            picker_files: Vec::new(),
            edit: None,
            delete_confirm: None,
            relink_open: false,
            relink_old: None,
            relink_new: None,
            relink_preview_count: None,
            relink_preview_running: false,
            relink_preview_error: None,
            relink_confirm: false,
            import_prompt: None,
        }
    }
}

impl CollectionsUi {
    /// Cached for the sidebar; this never reads SQLite.
    pub fn sidebar_collections(&self) -> &[Collection] {
        &self.collections
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Detail {
    Collection {
        id: CollectionId,
        kind: CollectionKind,
    },
    SavedSearch {
        name: String,
        query: String,
    },
}

impl Detail {
    fn key(&self) -> DetailKey {
        match self {
            Self::Collection { id, .. } => DetailKey::Collection(*id),
            Self::SavedSearch { name, .. } => DetailKey::SavedSearch(name.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DetailKey {
    Collection(CollectionId),
    SavedSearch(String),
}

enum UiMessage {
    Catalog(Result<(Vec<Collection>, Vec<SavedSearch>), String>),
    Items {
        key: DetailKey,
        result: Result<Vec<FileRecord>, String>,
    },
    OperationDone {
        result: Result<String, String>,
        refresh_catalog: bool,
        refresh_items: bool,
        close_edit: bool,
        close_delete: bool,
    },
    RelinkPreview {
        old: PathBuf,
        new: PathBuf,
        result: Result<u64, String>,
    },
    ImportPreview(Result<(String, ImportReport), String>),
}

struct ImportPrompt {
    json: String,
    conflicts: Vec<SmartCollectionConflict>,
    choices: HashMap<String, SmartCollectionConflictChoice>,
}

#[derive(Clone)]
enum EditKind {
    CreateManual { add_files: Vec<NewFile> },
    CreateSmart,
    CreateSavedSearch,
    RenameCollection(CollectionId),
    RenameSavedSearch(SavedSearch),
}

#[derive(Clone)]
struct EditDialog {
    kind: EditKind,
    name: String,
    query: String,
}

impl EditDialog {
    fn title(&self) -> &'static str {
        match &self.kind {
            EditKind::CreateManual { .. } => "New manual collection",
            EditKind::CreateSmart => "New smart collection",
            EditKind::CreateSavedSearch => "Save current search as",
            EditKind::RenameCollection(_) | EditKind::RenameSavedSearch(_) => "Rename",
        }
    }

    fn has_query(&self) -> bool {
        matches!(
            &self.kind,
            EditKind::CreateSmart | EditKind::CreateSavedSearch
        )
    }
}

#[derive(Clone)]
enum DeleteTarget {
    Collection { id: CollectionId, name: String },
    SavedSearch { name: String },
}

enum UiCommand {
    AddFiles {
        collection_id: CollectionId,
        files: Vec<NewFile>,
    },
    SaveEdit(EditDialog),
    Delete(DeleteTarget),
    RemoveItems {
        collection_id: CollectionId,
        file_ids: Vec<i64>,
    },
    ReorderItems {
        collection_id: CollectionId,
        file_ids: Vec<i64>,
    },
    ChooseExport {
        collection_id: CollectionId,
        name: String,
    },
    ChooseImport,
    ResolveImport {
        json: String,
        choices: HashMap<String, SmartCollectionConflictChoice>,
    },
    CancelImport,
    ChooseRelinkOld,
    ChooseRelinkNew,
    PreviewRelink {
        old: PathBuf,
        new: PathBuf,
    },
    ConfirmRelink {
        old: PathBuf,
        new: PathBuf,
    },
    Navigate(PathBuf),
    CaptureSelection,
}

#[derive(Clone, Copy)]
struct OperationFlags {
    refresh_catalog: bool,
    refresh_items: bool,
    close_edit: bool,
    close_delete: bool,
}

impl OperationFlags {
    const ITEMS: Self = Self {
        refresh_catalog: true,
        refresh_items: true,
        close_edit: false,
        close_delete: false,
    };
    const EDIT: Self = Self {
        refresh_catalog: true,
        refresh_items: false,
        close_edit: true,
        close_delete: false,
    };
    const DELETE: Self = Self {
        refresh_catalog: true,
        refresh_items: true,
        close_edit: false,
        close_delete: true,
    };
    const IMPORT: Self = Self {
        refresh_catalog: true,
        refresh_items: false,
        close_edit: false,
        close_delete: false,
    };
    const EXPORT: Self = Self {
        refresh_catalog: false,
        refresh_items: false,
        close_edit: false,
        close_delete: false,
    };
    const RELINK: Self = Self {
        refresh_catalog: false,
        refresh_items: true,
        close_edit: false,
        close_delete: false,
    };
}

/// Called every frame.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    poll_messages(app);
    if app.ops.library.db.is_some()
        && !app.ops.collections_ui.catalog_loaded
        && !app.ops.collections_ui.catalog_loading
    {
        start_catalog_load(app, ctx);
    }
    if app.ops.collections_ui.detail_requested {
        start_detail_load(app, ctx);
    }

    let search_query = app.ops.library.search.query.clone();
    let library_status = if app.ops.library.db.is_some() {
        None
    } else {
        Some(app.ops.library.open_error.clone().map_or_else(
            || "Opening library database…".to_string(),
            |e| format!("Library unavailable: {e}"),
        ))
    };
    let mut commands = Vec::new();

    if app.ops.collections_ui.open {
        let mut open = app.ops.collections_ui.open;
        egui::Window::new("Collections & saved searches")
            .open(&mut open)
            .default_size([940.0, 640.0])
            .resizable(true)
            .show(ctx, |ui| {
                render_main(
                    ui,
                    &mut app.ops.collections_ui,
                    &search_query,
                    library_status.as_deref(),
                    &mut commands,
                );
            });
        app.ops.collections_ui.open = open;
    }

    render_picker(ctx, &mut app.ops.collections_ui, &mut commands);
    render_edit_dialog(ctx, &mut app.ops.collections_ui, &mut commands);
    render_delete_dialog(ctx, &mut app.ops.collections_ui, &mut commands);
    render_relink_dialog(ctx, &mut app.ops.collections_ui, &mut commands);
    render_import_dialog(ctx, &mut app.ops.collections_ui, &mut commands);

    for command in commands {
        execute_command(app, ctx, command);
    }
}

/// `Cmd::AddToCollection`: capture selected file rows and show the small picker.
pub fn add_selection(app: &mut ZxrawApp) {
    let targets: std::collections::HashSet<PathBuf> =
        app.pane().action_targets().into_iter().collect();
    let files: Vec<NewFile> = app
        .pane()
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File && targets.contains(&entry.path))
        .map(NewFile::from)
        .collect();
    if files.is_empty() {
        app.toast("Select one or more files first");
        return;
    }
    let state = &mut app.ops.collections_ui;
    state.picker_files = files;
    state.picker_filter.clear();
    state.picker_open = true;
}

/// Open the management window on a collection. Used by the sidebar.
pub fn open_collection(app: &mut ZxrawApp, id: CollectionId) {
    let kind = app
        .ops
        .collections_ui
        .collections
        .iter()
        .find(|c| c.id == id)
        .map_or(CollectionKind::Manual, |c| c.kind);
    let state = &mut app.ops.collections_ui;
    state.open = true;
    set_detail(state, Detail::Collection { id, kind });
}

fn set_detail(state: &mut CollectionsUi, detail: Detail) {
    if state.detail.as_ref() == Some(&detail) {
        state.detail_requested = true;
        return;
    }
    state.detail = Some(detail);
    state.rows.clear();
    state.rows_error = None;
    state.rows_loading = false;
    state.detail_requested = true;
}

fn poll_messages(app: &mut ZxrawApp) {
    let mut toasts = Vec::new();
    {
        let state = &mut app.ops.collections_ui;
        while let Ok(message) = state.rx.try_recv() {
            match message {
                UiMessage::Catalog(result) => {
                    state.catalog_loading = false;
                    state.catalog_loaded = true;
                    match result {
                        Ok((collections, saved_searches)) => {
                            state.collections = collections;
                            state.saved_searches = saved_searches;
                        }
                        Err(error) => state.feedback = Some((error, true)),
                    }
                }
                UiMessage::Items { key, result } => {
                    if state.detail.as_ref().is_some_and(|d| d.key() == key) {
                        state.rows_loading = false;
                        match result {
                            Ok(rows) => {
                                state.rows = rows;
                                state.rows_error = None;
                            }
                            Err(error) => {
                                state.rows.clear();
                                state.rows_error = Some(error);
                            }
                        }
                    }
                }
                UiMessage::OperationDone {
                    result,
                    refresh_catalog,
                    refresh_items,
                    close_edit,
                    close_delete,
                } => {
                    state.operation_pending = false;
                    if refresh_catalog {
                        state.catalog_loaded = false;
                    }
                    if refresh_items && state.detail.is_some() {
                        state.detail_requested = true;
                    }
                    match result {
                        Ok(text) => {
                            state.feedback = Some((text.clone(), false));
                            toasts.push((text, false));
                            if close_edit {
                                state.edit = None;
                            }
                            if close_delete {
                                state.delete_confirm = None;
                            }
                        }
                        Err(error) => {
                            state.feedback = Some((error.clone(), true));
                            toasts.push((error, true));
                        }
                    }
                }
                UiMessage::RelinkPreview { old, new, result } => {
                    state.relink_preview_running = false;
                    if state.relink_old.as_ref() == Some(&old)
                        && state.relink_new.as_ref() == Some(&new)
                    {
                        match result {
                            Ok(count) => {
                                state.relink_preview_count = Some(count);
                                state.relink_preview_error = None;
                            }
                            Err(error) => {
                                state.relink_preview_count = None;
                                state.relink_preview_error = Some(error);
                            }
                        }
                    }
                }
                UiMessage::ImportPreview(result) => {
                    state.operation_pending = false;
                    match result {
                        Ok((json, report)) if !report.smart_conflicts.is_empty() => {
                            state.import_prompt = Some(ImportPrompt {
                                json,
                                conflicts: report.smart_conflicts,
                                choices: HashMap::new(),
                            });
                        }
                        Ok((_json, report)) => {
                            state.catalog_loaded = false;
                            let text = import_summary(&report);
                            state.feedback = Some((text.clone(), false));
                            toasts.push((text, false));
                        }
                        Err(error) => {
                            state.feedback = Some((error.clone(), true));
                            toasts.push((error, true));
                        }
                    }
                }
            }
        }
    }
    for (text, error) in toasts {
        if error {
            app.toast_err(text);
        } else {
            app.toast(text);
        }
    }
}

fn start_catalog_load(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(db) = app.ops.library.db.clone() else {
        return;
    };
    let state = &mut app.ops.collections_ui;
    if state.catalog_loading || state.catalog_loaded {
        return;
    }
    state.catalog_loading = true;
    let tx = state.tx.clone();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("collections-catalog".into())
        .spawn(move || {
            let result = (|| {
                Ok::<_, String>((
                    db.collections().map_err(|e| e.to_string())?,
                    db.saved_searches().map_err(|e| e.to_string())?,
                ))
            })();
            let _ = tx.send(UiMessage::Catalog(result));
            ctx.request_repaint();
        });
}

fn start_detail_load(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(db) = app.ops.library.db.clone() else {
        return;
    };
    let state = &mut app.ops.collections_ui;
    let Some(detail) = state.detail.clone() else {
        state.detail_requested = false;
        return;
    };
    state.detail_requested = false;
    state.rows_loading = true;
    state.rows_error = None;
    let key = detail.key();
    let tx = state.tx.clone();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("collections-items".into())
        .spawn(move || {
            let result = match detail {
                Detail::Collection { id, kind } => {
                    let opts = SearchOptions {
                        include_offline: false,
                        limit: if kind == CollectionKind::Manual {
                            None
                        } else {
                            Some(SEARCH_RESULT_LIMIT)
                        },
                        ..Default::default()
                    };
                    db.collection_files(id, &opts).map_err(|e| e.to_string())
                }
                Detail::SavedSearch { query, .. } => db
                    .search_str(
                        &query,
                        &SearchOptions {
                            limit: Some(SEARCH_RESULT_LIMIT),
                            ..Default::default()
                        },
                    )
                    .map_err(|e| e.to_string()),
            };
            let _ = tx.send(UiMessage::Items { key, result });
            ctx.request_repaint();
        });
}

fn start_operation(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    flags: OperationFlags,
    work: impl FnOnce(Arc<Db>) -> Result<String, String> + Send + 'static,
) {
    if app.ops.collections_ui.operation_pending {
        return;
    }
    let Some(db) = app.ops.library.db.clone() else {
        app.ops.collections_ui.feedback =
            Some(("Library database is not available yet".into(), true));
        return;
    };
    let state = &mut app.ops.collections_ui;
    state.operation_pending = true;
    let tx = state.tx.clone();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("collections-operation".into())
        .spawn(move || {
            let result = work(db);
            let _ = tx.send(UiMessage::OperationDone {
                result,
                refresh_catalog: flags.refresh_catalog,
                refresh_items: flags.refresh_items,
                close_edit: flags.close_edit,
                close_delete: flags.close_delete,
            });
            ctx.request_repaint();
        });
}

fn start_import_preview(app: &mut ZxrawApp, ctx: &egui::Context, path: PathBuf) {
    if app.ops.collections_ui.operation_pending {
        return;
    }
    let Some(db) = app.ops.library.db.clone() else {
        app.ops.collections_ui.feedback =
            Some(("Library database is not available yet".into(), true));
        return;
    };
    let state = &mut app.ops.collections_ui;
    state.operation_pending = true;
    let tx = state.tx.clone();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("collections-import-preview".into())
        .spawn(move || {
            let result = std::fs::read_to_string(&path)
                .map_err(|e| format!("Could not read import file: {e}"))
                .and_then(|json| {
                    db.import_collections_json(&json)
                        .map(|report| (json, report))
                        .map_err(|e| e.to_string())
                });
            let _ = tx.send(UiMessage::ImportPreview(result));
            ctx.request_repaint();
        });
}

fn import_summary(report: &ImportReport) -> String {
    format!(
        "Imported {} collection(s); linked {}, offline placeholders {}, merged {}",
        report.collections_created,
        report.items_linked,
        report.items_placeholder,
        report.collections_merged
    )
}

fn relink_summary(report: &RelinkReport) -> String {
    let mut summary = format!(
        "Relinked {} path(s), merged {} indexed row(s)",
        report.moved, report.merged
    );
    if !report.offline_paths.is_empty() {
        let paths = report
            .offline_paths
            .iter()
            .take(3)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let extra = report.offline_paths.len().saturating_sub(3);
        summary.push_str(&format!(
            "; {} destination path(s) remain offline: {paths}{}",
            report.offline_paths.len(),
            if extra > 0 {
                format!(" and {extra} more")
            } else {
                String::new()
            }
        ));
    }
    if !report.conflicts.is_empty() {
        summary.push_str(&format!(
            "; {} annotation conflict(s), source values kept",
            report.conflicts.len()
        ));
        for conflict in report.conflicts.iter().take(3) {
            summary.push_str(&format!(
                "; {}: {} → {}",
                conflict.field,
                conflict.source.display(),
                conflict.destination.display()
            ));
        }
        let extra = report.conflicts.len().saturating_sub(3);
        if extra > 0 {
            summary.push_str(&format!("; and {extra} more conflict(s)"));
        }
    }
    summary
}

fn atomic_export(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "export path has no file name",
        )
    })?;
    let (temporary, mut file) = loop {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.zxraw-export-{}-{sequence}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn render_main(
    ui: &mut egui::Ui,
    state: &mut CollectionsUi,
    search_query: &str,
    library_status: Option<&str>,
    commands: &mut Vec<UiCommand>,
) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("New manual collection").clicked() {
            state.edit = Some(EditDialog {
                kind: EditKind::CreateManual {
                    add_files: Vec::new(),
                },
                name: String::new(),
                query: String::new(),
            });
        }
        if ui.button("New smart collection").clicked() {
            state.edit = Some(EditDialog {
                kind: EditKind::CreateSmart,
                name: String::new(),
                query: String::new(),
            });
        }
        if ui
            .add_enabled(
                !search_query.trim().is_empty(),
                egui::Button::new("Save current search as…"),
            )
            .clicked()
        {
            state.edit = Some(EditDialog {
                kind: EditKind::CreateSavedSearch,
                name: String::new(),
                query: search_query.to_string(),
            });
        }
        if ui.button("Import…").clicked() {
            commands.push(UiCommand::ChooseImport);
        }
        if ui.button("Relink…").clicked() {
            state.relink_open = true;
        }
    });

    if state.operation_pending {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Saving changes…");
        });
    }
    if state.catalog_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading collections…");
        });
    }
    if let Some(status) = library_status {
        ui.label(RichText::new(status).weak());
    }
    if let Some((message, is_error)) = &state.feedback {
        let text = RichText::new(message).color(if *is_error {
            ui.visuals().error_fg_color
        } else {
            ui.visuals().text_color()
        });
        ui.label(text);
    }
    ui.separator();

    ui.columns(2, |columns| {
        render_catalog(&mut columns[0], state, commands);
        columns[0].separator();
        columns[0].label(RichText::new("Saved searches").strong());
        render_saved_searches(&mut columns[0], state);
        columns[1].vertical(|ui| render_detail(ui, state, commands));
    });
}

fn render_catalog(ui: &mut egui::Ui, state: &mut CollectionsUi, commands: &mut Vec<UiCommand>) {
    ui.label(RichText::new("Collections").strong());
    egui::ScrollArea::vertical()
        .id_salt("collections-catalog")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for collection in state.collections.clone() {
                let count = if collection.kind == CollectionKind::Manual {
                    format!("{} items", collection.item_count)
                } else {
                    "Smart".to_string()
                };
                let selected = matches!(&state.detail, Some(Detail::Collection { id, .. }) if *id == collection.id);
                let response = ui.selectable_label(selected, format!("{}  ·  {count}", collection.name));
                if response.clicked() {
                    set_detail(
                        state,
                        Detail::Collection {
                            id: collection.id,
                            kind: collection.kind,
                        },
                    );
                }
                response.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        state.edit = Some(EditDialog {
                            kind: EditKind::RenameCollection(collection.id),
                            name: collection.name.clone(),
                            query: String::new(),
                        });
                        ui.close();
                    }
                    if ui.button("Export to JSON…").clicked() {
                        commands.push(UiCommand::ChooseExport {
                            collection_id: collection.id,
                            name: collection.name.clone(),
                        });
                        ui.close();
                    }
                    if ui.button("Delete…").clicked() {
                        state.delete_confirm = Some(DeleteTarget::Collection {
                            id: collection.id,
                            name: collection.name.clone(),
                        });
                        ui.close();
                    }
                });
            }
            if state.collections.is_empty() && !state.catalog_loading {
                ui.label(RichText::new("No collections yet").weak());
            }
        });
}

fn render_saved_searches(ui: &mut egui::Ui, state: &mut CollectionsUi) {
    egui::ScrollArea::vertical()
        .id_salt("saved-search-catalog")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for saved in state.saved_searches.clone() {
                let selected = matches!(&state.detail, Some(Detail::SavedSearch { name, .. }) if name == &saved.name);
                let response = ui.selectable_label(selected, &saved.name);
                if response.clicked() {
                    set_detail(
                        state,
                        Detail::SavedSearch {
                            name: saved.name.clone(),
                            query: saved.query.clone(),
                        },
                    );
                }
                response.on_hover_text(&saved.query).context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        state.edit = Some(EditDialog {
                            kind: EditKind::RenameSavedSearch(saved.clone()),
                            name: saved.name.clone(),
                            query: String::new(),
                        });
                        ui.close();
                    }
                    if ui.button("Delete…").clicked() {
                        state.delete_confirm = Some(DeleteTarget::SavedSearch {
                            name: saved.name.clone(),
                        });
                        ui.close();
                    }
                });
            }
            if state.saved_searches.is_empty() && !state.catalog_loading {
                ui.label(RichText::new("No saved searches yet").weak());
            }
        });
}

fn render_detail(ui: &mut egui::Ui, state: &CollectionsUi, commands: &mut Vec<UiCommand>) {
    let Some(detail) = state.detail.as_ref() else {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.label(RichText::new("Choose a collection or saved search").weak());
        });
        return;
    };
    let (title, manual, query) = match detail {
        Detail::Collection { id, kind } => {
            let Some(collection) = state.collections.iter().find(|c| c.id == *id) else {
                ui.label(RichText::new("Collection is not in the current library list").weak());
                return;
            };
            (
                collection.name.clone(),
                *kind == CollectionKind::Manual,
                collection.query.clone(),
            )
        }
        Detail::SavedSearch { name, query } => (name.clone(), false, Some(query.clone())),
    };
    ui.heading(&title);
    if let Some(query) = query {
        ui.label(RichText::new(query).small().weak());
    }
    ui.horizontal(|ui| {
        if manual && ui.button("Add current selection…").clicked() {
            commands.push(UiCommand::CaptureSelection);
        }
        if let Detail::Collection { id, .. } = detail
            && ui.button("Export…").clicked()
        {
            commands.push(UiCommand::ChooseExport {
                collection_id: *id,
                name: title.clone(),
            });
        }
    });
    ui.separator();

    if state.rows_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Running query…");
        });
    }
    if let Some(error) = &state.rows_error {
        ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
    }
    if !state.rows_loading && state.rows_error.is_none() {
        ui.label(
            RichText::new(format!("{} item(s)", state.rows.len()))
                .small()
                .weak(),
        );
    }

    let rows = &state.rows;
    let current_ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut navigate = None;
    egui::ScrollArea::vertical()
        .id_salt("collection-items")
        .auto_shrink([false, false])
        .show_rows(ui, 44.0, rows.len(), |ui, range| {
            for index in range {
                let row = &rows[index];
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        if row.present {
                            let response = ui.selectable_label(false, RichText::new(&row.name));
                            if response.clicked() {
                                navigate = Some(row.path.clone());
                            }
                            response.on_hover_text(row.path.display().to_string());
                        } else {
                            // Offline/missing rows keep their last known path but cannot be opened.
                            ui.label(
                                RichText::new(format!("{}  ·  offline / missing", row.name))
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .on_hover_text("Not available on this machine right now");
                        }
                        ui.label(RichText::new(row.path.display().to_string()).small().weak());
                    });
                    if manual {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                commands.push(UiCommand::RemoveItems {
                                    collection_id: match detail {
                                        Detail::Collection { id, .. } => *id,
                                        Detail::SavedSearch { .. } => unreachable!(),
                                    },
                                    file_ids: vec![row.id],
                                });
                            }
                            if ui.small_button("↓").clicked() {
                                commands.push(UiCommand::ReorderItems {
                                    collection_id: match detail {
                                        Detail::Collection { id, .. } => *id,
                                        Detail::SavedSearch { .. } => unreachable!(),
                                    },
                                    file_ids: move_file_id(&current_ids, row.id, 1),
                                });
                            }
                            if ui.small_button("↑").clicked() {
                                commands.push(UiCommand::ReorderItems {
                                    collection_id: match detail {
                                        Detail::Collection { id, .. } => *id,
                                        Detail::SavedSearch { .. } => unreachable!(),
                                    },
                                    file_ids: move_file_id(&current_ids, row.id, -1),
                                });
                            }
                        });
                    }
                });
            }
        });
    if let Some(path) = navigate {
        commands.push(UiCommand::Navigate(path));
    }
}

fn render_picker(ctx: &egui::Context, state: &mut CollectionsUi, commands: &mut Vec<UiCommand>) {
    if !state.picker_open {
        return;
    }
    let mut open = state.picker_open;
    let mut close_requested = false;
    egui::Window::new("Add to collection")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(390.0)
        .show(ctx, |ui| {
            ui.label(format!("Add {} selected file(s)", state.picker_files.len()));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.picker_filter)
                        .hint_text("Filter collections…")
                        .desired_width(240.0),
                );
                if ui.button("New collection…").clicked() {
                    state.edit = Some(EditDialog {
                        kind: EditKind::CreateManual {
                            add_files: state.picker_files.clone(),
                        },
                        name: String::new(),
                        query: String::new(),
                    });
                    state.picker_open = false;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for collection in filtered_collections(&state.collections, &state.picker_filter)
                    {
                        let label =
                            format!("{}  ·  {} items", collection.name, collection.item_count);
                        if ui.button(label).clicked() {
                            commands.push(UiCommand::AddFiles {
                                collection_id: collection.id,
                                files: state.picker_files.clone(),
                            });
                            state.picker_open = false;
                        }
                    }
                    if filtered_collections(&state.collections, &state.picker_filter).is_empty() {
                        ui.label(RichText::new("No matching manual collections").weak());
                    }
                });
            if ui.button("Cancel").clicked() {
                close_requested = true;
            }
        });
    if close_requested {
        open = false;
    }
    if !open {
        state.picker_open = false;
    }
    if !state.picker_open {
        state.picker_files.clear();
    }
}

fn render_edit_dialog(
    ctx: &egui::Context,
    state: &mut CollectionsUi,
    commands: &mut Vec<UiCommand>,
) {
    let Some(form) = state.edit.as_mut() else {
        return;
    };
    let title = form.title();
    let mut open = true;
    let mut submit = false;
    let mut close_requested = false;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .show(ctx, |ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut form.name);
            if form.has_query() {
                ui.add_space(6.0);
                ui.label("Query");
                ui.add(
                    egui::TextEdit::multiline(&mut form.query)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("kind:raw rating:>=4"),
                );
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!state.operation_pending, egui::Button::new("Save"))
                    .clicked()
                {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close_requested = true;
                }
            });
        });
    if close_requested {
        open = false;
    }
    if submit {
        commands.push(UiCommand::SaveEdit(form.clone()));
    } else if !open {
        state.edit = None;
    }
}

fn render_delete_dialog(
    ctx: &egui::Context,
    state: &mut CollectionsUi,
    commands: &mut Vec<UiCommand>,
) {
    let Some(target) = state.delete_confirm.as_ref() else {
        return;
    };
    let target = target.clone();
    let (title, message) = match &target {
        DeleteTarget::Collection { name, .. } => (
            "Delete collection?",
            format!("Delete '{name}' and its saved membership? The indexed files will remain."),
        ),
        DeleteTarget::SavedSearch { name } => ("Delete saved search?", format!("Delete '{name}'?")),
    };
    let mut open = true;
    let mut confirm = false;
    let mut close_requested = false;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(message);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!state.operation_pending, egui::Button::new("Delete"))
                    .clicked()
                {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() {
                    close_requested = true;
                }
            });
        });
    if close_requested {
        open = false;
    }
    if confirm {
        commands.push(UiCommand::Delete(target));
    } else if !open {
        state.delete_confirm = None;
    }
}

fn render_relink_dialog(
    ctx: &egui::Context,
    state: &mut CollectionsUi,
    commands: &mut Vec<UiCommand>,
) {
    if !state.relink_open {
        return;
    }
    let mut open = state.relink_open;
    let mut close_requested = false;
    egui::Window::new("Relink missing items")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(560.0)
        .show(ctx, |ui| {
            ui.label("Choose the old folder prefix and its new location.");
            ui.horizontal(|ui| {
                if ui.button("Choose old folder…").clicked() {
                    commands.push(UiCommand::ChooseRelinkOld);
                }
                ui.label(
                    state
                        .relink_old
                        .as_ref()
                        .map_or_else(|| "No old folder".to_string(), |p| p.display().to_string()),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Choose new folder…").clicked() {
                    commands.push(UiCommand::ChooseRelinkNew);
                }
                ui.label(
                    state
                        .relink_new
                        .as_ref()
                        .map_or_else(|| "No new folder".to_string(), |p| p.display().to_string()),
                );
            });
            ui.horizontal(|ui| {
                let ready = state.relink_old.is_some()
                    && state.relink_new.is_some()
                    && !state.relink_preview_running;
                if ui
                    .add_enabled(ready, egui::Button::new("Preview matches"))
                    .clicked()
                    && let (Some(old), Some(new)) =
                        (state.relink_old.clone(), state.relink_new.clone())
                {
                    commands.push(UiCommand::PreviewRelink { old, new });
                }
                if state.relink_preview_running {
                    ui.spinner();
                    ui.label("Counting indexed paths…");
                } else if let Some(count) = state.relink_preview_count {
                    ui.label(format!("{count} indexed item(s) will be relinked"));
                }
            });
            if let Some(error) = &state.relink_preview_error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }
            ui.separator();
            let count = state.relink_preview_count.unwrap_or(0);
            let can_relink = count > 0 && !state.operation_pending;
            ui.horizontal(|ui| {
                if state.relink_confirm {
                    ui.label(format!(
                        "Rewrite {count} indexed path(s) from the old folder to the new one?"
                    ));
                    if ui
                        .add_enabled(
                            !state.operation_pending,
                            egui::Button::new("Confirm relink"),
                        )
                        .clicked()
                    {
                        if let (Some(old), Some(new)) =
                            (state.relink_old.clone(), state.relink_new.clone())
                        {
                            commands.push(UiCommand::ConfirmRelink { old, new });
                        }
                        state.relink_confirm = false;
                    }
                    if ui.button("Back").clicked() {
                        state.relink_confirm = false;
                    }
                } else {
                    if ui
                        .add_enabled(
                            can_relink,
                            egui::Button::new(format!("Relink {count} item(s)…")),
                        )
                        .clicked()
                    {
                        state.relink_confirm = true;
                    }
                }
                if ui.button("Close").clicked() {
                    close_requested = true;
                }
            });
        });
    if close_requested {
        open = false;
    }
    if !open {
        state.relink_confirm = false;
    }
    state.relink_open = open;
}

fn render_import_dialog(
    ctx: &egui::Context,
    state: &mut CollectionsUi,
    commands: &mut Vec<UiCommand>,
) {
    let Some(prompt) = state.import_prompt.as_mut() else {
        return;
    };
    let mut open = true;
    let mut apply = false;
    let mut cancel = false;
    egui::Window::new("Smart collection import conflicts")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .show(ctx, |ui| {
            ui.label("These collections already exist with a different query. Choose how each one should be imported.");
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for conflict in &prompt.conflicts {
                    ui.group(|ui| {
                        ui.label(RichText::new(&conflict.name).strong());
                        ui.label(format!("Existing: {}", conflict.existing_query));
                        ui.label(format!("Import: {}", conflict.imported_query));
                        let key = conflict.name.to_lowercase();
                        let mut selected = prompt.choices.get(&key).copied();
                        ui.horizontal_wrapped(|ui| {
                            ui.selectable_value(
                                &mut selected,
                                Some(SmartCollectionConflictChoice::Replace),
                                "Replace",
                            );
                            ui.selectable_value(
                                &mut selected,
                                Some(SmartCollectionConflictChoice::KeepExisting),
                                "Keep existing",
                            );
                            ui.selectable_value(
                                &mut selected,
                                Some(SmartCollectionConflictChoice::ImportAsCopy),
                                format!("Import as copy (e.g. '{} (2)')", conflict.name),
                            );
                        });
                        if let Some(choice) = selected {
                            prompt.choices.insert(key, choice);
                        } else {
                            prompt.choices.remove(&key);
                        }
                    });
                }
            });
            let all_chosen = prompt
                .conflicts
                .iter()
                .all(|conflict| prompt.choices.contains_key(&conflict.name.to_lowercase()));
            ui.separator();
            ui.horizontal(|ui| {
                apply = ui
                    .add_enabled(all_chosen, egui::Button::new("Apply choices and import"))
                    .clicked();
                cancel = ui.button("Cancel import").clicked();
            });
        });
    if apply {
        commands.push(UiCommand::ResolveImport {
            json: prompt.json.clone(),
            choices: prompt.choices.clone(),
        });
    } else if cancel || !open {
        commands.push(UiCommand::CancelImport);
    }
}

fn execute_command(app: &mut ZxrawApp, ctx: &egui::Context, command: UiCommand) {
    match command {
        UiCommand::AddFiles {
            collection_id,
            files,
        } => {
            start_operation(app, ctx, OperationFlags::ITEMS, move |db| {
                let ids = db.upsert_files(&files).map_err(|e| e.to_string())?;
                let added = db
                    .add_to_collection(collection_id, &ids)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Added {added} file(s) to the collection"))
            });
        }
        UiCommand::SaveEdit(form) => start_save_edit(app, ctx, form),
        UiCommand::Delete(target) => start_delete(app, ctx, target),
        UiCommand::RemoveItems {
            collection_id,
            file_ids,
        } => {
            start_operation(app, ctx, OperationFlags::ITEMS, move |db| {
                let removed = db
                    .remove_from_collection(collection_id, &file_ids)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Removed {removed} item(s) from the collection"))
            });
        }
        UiCommand::ReorderItems {
            collection_id,
            file_ids,
        } => {
            start_operation(app, ctx, OperationFlags::ITEMS, move |db| {
                db.reorder_collection(collection_id, &file_ids)
                    .map_err(|e| e.to_string())?;
                Ok("Collection order updated".into())
            });
        }
        UiCommand::ChooseExport {
            collection_id,
            name,
        } => {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .set_file_name(format!("{name}.json"))
                .save_file()
            {
                start_operation(app, ctx, OperationFlags::EXPORT, move |db| {
                    let json = db
                        .export_collections_json(&[collection_id])
                        .map_err(|e| e.to_string())?;
                    atomic_export(&path, json.as_bytes())
                        .map_err(|e| format!("Could not write export: {e}"))?;
                    Ok(format!("Exported collection to {}", path.display()))
                });
            }
        }
        UiCommand::ChooseImport => {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                start_import_preview(app, ctx, path);
            }
        }
        UiCommand::ResolveImport { json, choices } => {
            app.ops.collections_ui.import_prompt = None;
            start_operation(app, ctx, OperationFlags::IMPORT, move |db| {
                let report = db
                    .import_collections_json_with_choices(&json, &choices)
                    .map_err(|e| e.to_string())?;
                if !report.smart_conflicts.is_empty() {
                    return Err("Some smart collection conflicts still need a choice".into());
                }
                Ok(import_summary(&report))
            });
        }
        UiCommand::CancelImport => app.ops.collections_ui.import_prompt = None,
        UiCommand::ChooseRelinkOld => {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                let state = &mut app.ops.collections_ui;
                state.relink_old = Some(path);
                clear_relink_preview(state);
            }
        }
        UiCommand::ChooseRelinkNew => {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                let state = &mut app.ops.collections_ui;
                state.relink_new = Some(path);
                clear_relink_preview(state);
            }
        }
        UiCommand::PreviewRelink { old, new } => start_relink_preview(app, ctx, old, new),
        UiCommand::ConfirmRelink { old, new } => {
            clear_relink_preview(&mut app.ops.collections_ui);
            start_operation(app, ctx, OperationFlags::RELINK, move |db| {
                let report: RelinkReport =
                    db.relink(&old, &new, None).map_err(|e| e.to_string())?;
                Ok(relink_summary(&report))
            });
            app.ops.collections_ui.relink_open = false;
        }
        UiCommand::Navigate(path) => app.navigate(path),
        UiCommand::CaptureSelection => add_selection(app),
    }
}

fn start_save_edit(app: &mut ZxrawApp, ctx: &egui::Context, form: EditDialog) {
    let EditDialog { kind, name, query } = form;
    let name = name.trim().to_string();
    start_operation(app, ctx, OperationFlags::EDIT, move |db| match kind {
        EditKind::CreateManual { add_files } => {
            let id = db
                .create_manual_collection(&name)
                .map_err(|e| e.to_string())?;
            let added = if add_files.is_empty() {
                0
            } else {
                let ids = db.upsert_files(&add_files).map_err(|e| e.to_string())?;
                db.add_to_collection(id, &ids).map_err(|e| e.to_string())?
            };
            Ok(if added > 0 {
                format!("Created '{name}' and added {added} file(s)")
            } else {
                format!("Created collection '{name}'")
            })
        }
        EditKind::CreateSmart => {
            db.create_smart_collection(&name, &query)
                .map_err(|e| e.to_string())?;
            Ok(format!("Created smart collection '{name}'"))
        }
        EditKind::CreateSavedSearch => {
            db.save_search(&name, &query, None)
                .map_err(|e| e.to_string())?;
            Ok(format!("Saved search '{name}'"))
        }
        EditKind::RenameCollection(id) => {
            db.rename_collection(id, &name).map_err(|e| e.to_string())?;
            Ok(format!("Renamed collection to '{name}'"))
        }
        EditKind::RenameSavedSearch(saved) => {
            if !saved.name.eq_ignore_ascii_case(&name) {
                let exists = db
                    .saved_searches()
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|s| s.name.eq_ignore_ascii_case(&name));
                if exists {
                    return Err(format!("A saved search named '{name}' already exists"));
                }
                db.save_search(&name, &saved.query, saved.sort.as_deref())
                    .map_err(|e| e.to_string())?;
                db.delete_saved_search(&saved.name)
                    .map_err(|e| e.to_string())?;
            } else {
                db.save_search(&name, &saved.query, saved.sort.as_deref())
                    .map_err(|e| e.to_string())?;
            }
            Ok(format!("Renamed saved search to '{name}'"))
        }
    });
}

fn start_delete(app: &mut ZxrawApp, ctx: &egui::Context, target: DeleteTarget) {
    match &target {
        DeleteTarget::Collection { id, .. } => {
            if matches!(app.ops.collections_ui.detail, Some(Detail::Collection { id: active, .. }) if active == *id)
            {
                app.ops.collections_ui.detail = None;
                app.ops.collections_ui.rows.clear();
            }
        }
        DeleteTarget::SavedSearch { name } => {
            if matches!(&app.ops.collections_ui.detail, Some(Detail::SavedSearch { name: active, .. }) if active == name)
            {
                app.ops.collections_ui.detail = None;
                app.ops.collections_ui.rows.clear();
            }
        }
    }
    start_operation(app, ctx, OperationFlags::DELETE, move |db| match target {
        DeleteTarget::Collection { id, name } => {
            if db.delete_collection(id).map_err(|e| e.to_string())? {
                Ok(format!("Deleted collection '{name}'"))
            } else {
                Err(format!("Collection '{name}' no longer exists"))
            }
        }
        DeleteTarget::SavedSearch { name } => {
            if db.delete_saved_search(&name).map_err(|e| e.to_string())? {
                Ok(format!("Deleted saved search '{name}'"))
            } else {
                Err(format!("Saved search '{name}' no longer exists"))
            }
        }
    });
}

fn start_relink_preview(app: &mut ZxrawApp, ctx: &egui::Context, old: PathBuf, new: PathBuf) {
    let Some(db) = app.ops.library.db.clone() else {
        app.ops.collections_ui.relink_preview_error =
            Some("Library database is not available yet".into());
        return;
    };
    let state = &mut app.ops.collections_ui;
    state.relink_preview_running = true;
    state.relink_preview_count = None;
    state.relink_preview_error = None;
    let tx = state.tx.clone();
    let ctx = ctx.clone();
    let old_for_message = old.clone();
    let new_for_message = new.clone();
    let _ = std::thread::Builder::new()
        .name("collections-relink-preview".into())
        .spawn(move || {
            // Scope the SQL to the old folder's subtree; the exact prefix check below
            // still decides which rows relink() will actually rewrite.
            let result = db
                .search_str(
                    "is:offline OR is:online",
                    &SearchOptions {
                        include_offline: true,
                        limit: None,
                        scope: Some(SearchScope {
                            path: old.clone(),
                            recursive: true,
                        }),
                        ..Default::default()
                    },
                )
                .map(|rows| {
                    count_relink_matches(
                        &rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
                        &old,
                    )
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMessage::RelinkPreview {
                old: old_for_message,
                new: new_for_message,
                result,
            });
            ctx.request_repaint();
        });
}

fn clear_relink_preview(state: &mut CollectionsUi) {
    state.relink_preview_count = None;
    state.relink_preview_error = None;
    state.relink_confirm = false;
}

fn filtered_collections<'a>(collections: &'a [Collection], query: &str) -> Vec<&'a Collection> {
    let query = query.trim().to_lowercase();
    collections
        .iter()
        .filter(|collection| collection.kind == CollectionKind::Manual)
        .filter(|collection| query.is_empty() || collection.name.to_lowercase().contains(&query))
        .collect()
}

fn move_file_id(order: &[i64], id: i64, delta: isize) -> Vec<i64> {
    let mut reordered = order.to_vec();
    let Some(index) = reordered.iter().position(|candidate| *candidate == id) else {
        return reordered;
    };
    let target = index as isize + delta;
    if (0..reordered.len() as isize).contains(&target) {
        reordered.swap(index, target as usize);
    }
    reordered
}

fn count_relink_matches(paths: &[PathBuf], old: &Path) -> u64 {
    paths
        .iter()
        .filter(|path| path_matches_prefix(path, old))
        .count() as u64
}

fn path_matches_prefix(path: &Path, old: &Path) -> bool {
    let normalize = |path: &Path| {
        let mut text = path.to_string_lossy().replace('/', "\\");
        while text.len() > 3 && text.ends_with('\\') {
            text.pop();
        }
        text
    };
    let path = normalize(path);
    let old = normalize(old);
    if path.eq_ignore_ascii_case(&old) {
        return true;
    }
    let Some(prefix) = path.get(..old.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(&old) {
        return false;
    }
    old.ends_with('\\') || path.as_bytes().get(old.len()) == Some(&b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(id: i64, name: &str, kind: CollectionKind) -> Collection {
        Collection {
            id,
            name: name.into(),
            kind,
            query: (kind == CollectionKind::Smart).then(|| "kind:raw".into()),
            created_at: 0,
            updated_at: 0,
            item_count: 2,
        }
    }

    #[test]
    fn reorder_moves_one_item_and_keeps_boundary_items() {
        assert_eq!(move_file_id(&[1, 2, 3], 2, -1), [2, 1, 3]);
        assert_eq!(move_file_id(&[1, 2, 3], 2, 1), [1, 3, 2]);
        assert_eq!(move_file_id(&[1, 2, 3], 1, -1), [1, 2, 3]);
        assert_eq!(move_file_id(&[1, 2, 3], 99, 1), [1, 2, 3]);
    }

    #[test]
    fn relink_preview_counts_exact_prefix_and_not_siblings() {
        let paths = [
            PathBuf::from(r"E:\Shoot\a.cr3"),
            PathBuf::from(r"e:\shoot\sub\b.jpg"),
            PathBuf::from(r"E:\Shoot-2\c.jpg"),
            PathBuf::from(r"D:\Other\d.jpg"),
        ];
        assert_eq!(count_relink_matches(&paths, Path::new(r"E:\Shoot")), 2);
    }

    #[test]
    fn picker_filters_case_insensitively_and_only_offers_manual_collections() {
        let collections = [
            collection(1, "Wedding Selects", CollectionKind::Manual),
            collection(2, "Family", CollectionKind::Manual),
            collection(3, "Wedding raws", CollectionKind::Smart),
        ];
        let filtered = filtered_collections(&collections, "WEDD");
        assert_eq!(filtered.iter().map(|c| c.id).collect::<Vec<_>>(), [1]);
        assert_eq!(filtered_collections(&collections, "").len(), 2);
    }

    #[test]
    fn export_replaces_destination_only_after_complete_temp_write() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("collections.json");
        std::fs::write(&destination, b"old data").unwrap();

        atomic_export(&destination, b"complete json").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"complete json");
        assert_eq!(
            std::fs::read_dir(temp.path()).unwrap().count(),
            1,
            "temporary sibling should be renamed away"
        );
    }

    #[test]
    fn failed_export_keeps_existing_destination_intact() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        std::fs::create_dir(&destination).unwrap();

        assert!(atomic_export(&destination, b"data").is_err());

        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
