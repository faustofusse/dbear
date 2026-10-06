//! Dumps and restores: the options dialogs, and the jobs that run in the background (several at
//! once; closing a dialog doesn't stop them), shown as cards over the window's bottom-right corner.
//! The work itself is `dbcore::dump` / `dbcore::restore`, like the macOS app.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbcore::dump::{self, CancelToken, Compression, DataStyle, DumpContent, DumpOptions, DumpProgress, DumpScope, DumpSummary, ProgressFn};
use dbcore::restore::{self, RestoreOptions, RestoreProgress, RestoreSummary};
use dbcore::secrets::{self, KeyringSecretStore};
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Schema, TableInfo, TableKind};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::tabs::plural;

/// `config` with its saved password, read off the UI thread (the keyring can block or prompt).
async fn with_password(config: ConnectionConfig, secrets: Option<Arc<KeyringSecretStore>>, executor: BackgroundExecutor) -> ConnectionConfig {
    executor
        .spawn(async move {
            match secrets {
                Some(store) => secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config),
                None => config,
            }
        })
        .await
}

/// `12.3 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit < UNITS.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Today's date (UTC) as `2025-06-01`, for default file names.
fn today() -> String {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400) as i64;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn pointed_at(base: &ConnectionConfig, database: &str) -> ConnectionConfig {
    if database == base.default_database() { base.clone() } else { base.with_database(database) }
}

/// A row of a form: a label, then the control.
fn field(label: &str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .child(div().w(px(90.)).flex_shrink_0().text_sm().text_color(cx.theme().muted_foreground).child(label.to_string()))
        .child(div().flex_1().min_w_0().child(control))
}

/// A dropdown of `options`, `selected` checked; `pick` gets the chosen index.
fn picker<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: Vec<(T, String)>,
    selected: T,
    pick: impl Fn(T, &mut App) + 'static,
) -> impl IntoElement {
    let label = options.iter().find(|(v, _)| *v == selected).map(|(_, l)| l.clone()).unwrap_or_default();
    let pick = Rc::new(pick);
    Button::new(id).outline().small().label(label).dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
        for (value, title) in options.clone() {
            let pick = pick.clone();
            menu = menu.item(PopupMenuItem::new(title).checked(value == selected).on_click(move |_, _, cx| pick(value, cx)));
        }
        menu
    })
}

use std::rc::Rc;

// MARK: - Dump dialog

/// What "Dump…" preselects.
#[derive(Clone)]
pub enum DumpPreset {
    Database,
    Schema(String),
    Table(TableInfo),
}

enum Tables {
    Loading,
    Loaded(Vec<Schema>),
    Failed(String),
}

pub struct DumpDialog {
    /// The saved connection (no password), as stored.
    pub base: ConnectionConfig,
    /// Databases to pick from (servers that show all databases).
    databases: Vec<String>,
    database: String,
    everything: bool,
    tables: Tables,
    selected: BTreeSet<(String, String)>,
    content: DumpContent,
    compression: Compression,
    data_style: DataStyle,
    drop_objects: bool,
    create_database: bool,
    preset: DumpPreset,
    secrets: Option<Arc<KeyringSecretStore>>,
    _load: Option<Task<()>>,
}

impl DumpDialog {
    pub fn new(
        base: ConnectionConfig,
        database: String,
        databases: Vec<String>,
        preset: DumpPreset,
        secrets: Option<Arc<KeyringSecretStore>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut dialog = Self {
            base,
            databases,
            database,
            everything: matches!(preset, DumpPreset::Database),
            tables: Tables::Loading,
            selected: BTreeSet::new(),
            content: DumpContent::SchemaAndData,
            compression: Compression::None,
            data_style: DataStyle::Copy,
            drop_objects: false,
            create_database: false,
            preset,
            secrets,
            _load: None,
        };
        dialog.load_tables(cx);
        dialog
    }

    fn kind(&self) -> DatabaseKind {
        self.base.kind
    }

    /// The connection pointed at the database to dump (no password yet).
    pub fn target(&self) -> ConnectionConfig {
        pointed_at(&self.base, &self.database)
    }

    pub fn secrets(&self) -> Option<Arc<KeyringSecretStore>> {
        self.secrets.clone()
    }

    fn set_database(&mut self, database: String, cx: &mut Context<Self>) {
        if database != self.database {
            self.database = database;
            self.load_tables(cx);
        }
    }

    fn load_tables(&mut self, cx: &mut Context<Self>) {
        self.tables = Tables::Loading;
        let (target, secrets) = (self.target(), self.secrets.clone());
        self._load = Some(cx.spawn(async move |this, cx| {
            let config = with_password(target, secrets, cx.background_executor().clone()).await;
            let connection = Connection::new(config);
            let result = connection.list_schemas().await;
            connection.disconnect().await;
            this.update(cx, |this, cx| {
                this.tables = match result {
                    Ok(schemas) => {
                        this.apply_preset(&schemas);
                        Tables::Loaded(schemas)
                    }
                    Err(e) => Tables::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_preset(&mut self, schemas: &[Schema]) {
        let key = |t: &TableInfo| (t.schema.clone(), t.name.clone());
        self.selected = match &self.preset {
            DumpPreset::Database => BTreeSet::new(),
            DumpPreset::Schema(name) => schemas.iter().filter(|s| s.name == *name).flat_map(|s| s.tables.iter().map(key)).collect(),
            DumpPreset::Table(table) => BTreeSet::from([key(table)]),
        };
    }

    /// Whole database, whole schemas (with their functions, types…), or just some tables.
    fn scope(&self) -> DumpScope {
        let Tables::Loaded(schemas) = &self.tables else { return DumpScope::Database };
        if self.everything {
            return DumpScope::Database;
        }
        let picked = |t: &TableInfo| self.selected.contains(&(t.schema.clone(), t.name.clone()));
        let all: Vec<&TableInfo> = schemas.iter().flat_map(|s| &s.tables).collect();
        if all.iter().all(|t| picked(t)) && schemas.iter().all(|s| !s.tables.is_empty()) {
            return DumpScope::Database;
        }
        let whole: Vec<&Schema> = schemas.iter().filter(|s| !s.tables.is_empty() && s.tables.iter().all(picked)).collect();
        let rest = all.iter().any(|t| picked(t) && !whole.iter().any(|s| s.name == t.schema));
        if !rest {
            return DumpScope::Schemas(whole.iter().map(|s| s.name.clone()).collect());
        }
        DumpScope::Tables(all.into_iter().filter(|t| picked(t)).cloned().collect())
    }

    pub fn can_dump(&self) -> bool {
        self.everything || !self.selected.is_empty()
    }

    pub fn options(&self) -> DumpOptions {
        DumpOptions {
            content: self.content,
            scope: self.scope(),
            compression: self.compression,
            data_style: if self.kind() == DatabaseKind::Postgres { self.data_style } else { DataStyle::Insert },
            drop_objects: self.content != DumpContent::DataOnly && self.drop_objects,
            create_database: self.kind() == DatabaseKind::Mysql && self.content != DumpContent::DataOnly && self.create_database,
        }
    }

    pub fn file_name(&self) -> String {
        dump::default_file_name(&dump::target_name(&self.target()), &today(), self.compression)
    }

    fn toggle_table(&mut self, key: (String, String), cx: &mut Context<Self>) {
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
        cx.notify();
    }

    fn toggle_schema(&mut self, schema: &Schema, cx: &mut Context<Self>) {
        let keys: Vec<_> = schema.tables.iter().map(|t| (t.schema.clone(), t.name.clone())).collect();
        let all = keys.iter().all(|k| self.selected.contains(k));
        for key in keys {
            if all {
                self.selected.remove(&key);
            } else {
                self.selected.insert(key);
            }
        }
        cx.notify();
    }

    fn render_tables(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let schemas = match &self.tables {
            Tables::Loading => return h_flex().h(px(200.)).justify_center().child(Spinner::new()).into_any_element(),
            Tables::Failed(e) => return div().text_sm().text_color(theme.red).child(e.clone()).into_any_element(),
            Tables::Loaded(schemas) => schemas,
        };
        let mut list = v_flex().id("dump-tables").size_full().p_1().overflow_y_scrollbar();
        for (i, schema) in schemas.iter().enumerate() {
            let all = !schema.tables.is_empty()
                && schema.tables.iter().all(|t| self.selected.contains(&(t.schema.clone(), t.name.clone())));
            let toggle = schema.clone();
            list = list.child(
                Checkbox::new(("dump-schema", i))
                    .checked(all)
                    .label(schema.name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_schema(&toggle, cx))),
            );
            for (j, table) in schema.tables.iter().enumerate() {
                let key = (table.schema.clone(), table.name.clone());
                let checked = self.selected.contains(&key);
                let icon = if table.kind == TableKind::View { IconName::Eye } else { IconName::FileText };
                list = list.child(
                    h_flex()
                        .pl_6()
                        .gap_1()
                        .child(
                            Checkbox::new(SharedString::from(format!("dump-table-{i}-{j}")))
                                .checked(checked)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle_table(key.clone(), cx))),
                        )
                        .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
                        .child(div().text_sm().child(table.name.clone())),
                );
            }
        }
        div().h(px(200.)).border_1().border_color(theme.border).rounded_md().child(list).into_any_element()
    }
}

impl Render for DumpDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let kind = self.kind();
        let set = move |f: Box<dyn Fn(&mut DumpDialog)>| {
            let this = this.clone();
            move |cx: &mut App| {
                this.update(cx, |d, cx| {
                    f(d);
                    cx.notify();
                })
                .ok();
            }
        };
        let database = if self.databases.len() > 1 {
            let this = cx.entity().downgrade();
            let current = self.database.clone();
            let databases = self.databases.clone();
            Button::new("dump-database")
                .outline()
                .small()
                .label(current.clone())
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for db in &databases {
                        let (this, db) = (this.clone(), db.clone());
                        menu = menu.item(PopupMenuItem::new(db.clone()).checked(db == current).on_click(move |_, _, cx| {
                            let db = db.clone();
                            this.update(cx, |d, cx| d.set_database(db, cx)).ok();
                        }));
                    }
                    menu
                })
                .into_any_element()
        } else {
            div().text_sm().child(dump::target_name(&self.target())).into_any_element()
        };
        let everything = self.everything;
        let contents = RadioGroup::vertical("dump-what")
            .child(Radio::new("whole").label("Whole database"))
            .child(Radio::new("some").label("Selected schemas and tables"))
            .selected_index(Some(if everything { 0 } else { 1 }))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.everything = *ix == 0;
                cx.notify();
            }));
        let content = self.content;
        let include = picker(
            "dump-content",
            vec![
                (DumpContent::SchemaAndData, "Schema and data".into()),
                (DumpContent::SchemaOnly, "Schema only".into()),
                (DumpContent::DataOnly, "Data only".into()),
            ],
            content,
            {
                let set = set.clone();
                move |v, cx| set(Box::new(move |d| d.content = v))(cx)
            },
        );
        let format = picker(
            "dump-format",
            vec![(Compression::None, "SQL (.sql)".into()), (Compression::Gzip, "Gzipped SQL (.sql.gz)".into())],
            self.compression,
            {
                let set = set.clone();
                move |v, cx| set(Box::new(move |d| d.compression = v))(cx)
            },
        );
        let rows_as = (kind == DatabaseKind::Postgres && content != DumpContent::SchemaOnly).then(|| {
            picker(
                "dump-rows",
                vec![(DataStyle::Copy, "COPY (fast, psql)".into()), (DataStyle::Insert, "INSERT statements".into())],
                self.data_style,
                {
                    let set = set.clone();
                    move |v, cx| set(Box::new(move |d| d.data_style = v))(cx)
                },
            )
        });
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(cx.theme().muted_foreground).truncate().child(self.base.summary()))
            .child(field("Database", database, cx))
            .child(field("Dump", contents, cx))
            .when(!everything, |form| form.child(self.render_tables(cx)))
            .child(field("Include", include, cx))
            .child(field("Format", format, cx))
            .children(rows_as.map(|rows| field("Rows as", rows, cx)))
            .when(content != DumpContent::DataOnly, |form| {
                form.child(field(
                    "",
                    Checkbox::new("drop-objects")
                        .checked(self.drop_objects)
                        .label("Drop existing objects before creating them")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.drop_objects = *checked;
                            cx.notify();
                        })),
                    cx,
                ))
            })
            .when(kind == DatabaseKind::Mysql && content != DumpContent::DataOnly, |form| {
                form.child(field(
                    "",
                    Checkbox::new("create-database")
                        .checked(self.create_database)
                        .label("Include CREATE DATABASE and USE")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.create_database = *checked;
                            cx.notify();
                        })),
                    cx,
                ))
            })
    }
}

// MARK: - Restore dialog

pub struct RestoreDialog {
    pub base: ConnectionConfig,
    databases: Vec<String>,
    database: String,
    pub file: PathBuf,
    single_transaction: bool,
    stop_on_error: bool,
}

impl RestoreDialog {
    pub fn new(base: ConnectionConfig, database: String, databases: Vec<String>, file: PathBuf) -> Self {
        // MySQL commits schema changes as it goes: a transaction can't make it all or nothing.
        let single_transaction = base.kind != DatabaseKind::Mysql;
        Self { base, databases, database, file, single_transaction, stop_on_error: true }
    }

    pub fn target(&self) -> ConnectionConfig {
        pointed_at(&self.base, &self.database)
    }

    pub fn options(&self) -> RestoreOptions {
        let transaction = self.base.kind != DatabaseKind::Mysql && self.single_transaction;
        RestoreOptions { single_transaction: transaction, stop_on_error: transaction || self.stop_on_error }
    }

    /// The database's key in the workspace (connection id, database), to refresh it afterwards.
    pub fn database(&self) -> String {
        self.database.clone()
    }
}

impl Render for RestoreDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let size = std::fs::metadata(&self.file).map(|m| format_bytes(m.len())).unwrap_or_default();
        let file = self.file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let database = if self.databases.len() > 1 {
            let this = cx.entity().downgrade();
            let current = self.database.clone();
            let databases = self.databases.clone();
            Button::new("restore-database")
                .outline()
                .small()
                .label(current.clone())
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for db in &databases {
                        let (this, db) = (this.clone(), db.clone());
                        menu = menu.item(PopupMenuItem::new(db.clone()).checked(db == current).on_click(move |_, _, cx| {
                            let db = db.clone();
                            this.update(cx, |d, cx| {
                                d.database = db;
                                cx.notify();
                            })
                            .ok();
                        }));
                    }
                    menu
                })
                .into_any_element()
        } else {
            div().text_sm().child(dump::target_name(&self.target())).into_any_element()
        };
        let mysql = self.base.kind == DatabaseKind::Mysql;
        let transaction = !mysql && self.single_transaction;
        v_flex()
            .gap_3()
            .child(field("File", div().text_sm().truncate().child(format!("{file} · {size}")), cx))
            .child(field("Connection", div().text_sm().child(self.base.name.clone()), cx))
            .child(field("Database", database, cx))
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "Every statement in the file runs against this database. Objects with the same names may be replaced, \
                 or make the restore fail. To restore into a new database, create it first.",
            ))
            .when(!mysql, |form| {
                form.child(
                    Checkbox::new("single-transaction")
                        .checked(self.single_transaction)
                        .label("All or nothing (one transaction)")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.single_transaction = *checked;
                            cx.notify();
                        })),
                )
            })
            .child(
                Checkbox::new("stop-on-error")
                    .checked(transaction || self.stop_on_error)
                    .disabled(transaction)
                    .label("Stop at the first error")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.stop_on_error = *checked;
                        cx.notify();
                    })),
            )
    }
}

// MARK: - Jobs

#[derive(Clone, Copy, PartialEq)]
enum JobKind {
    Dump,
    Restore,
}

enum JobState {
    Running,
    Finished(String),
    Failed(String),
    Cancelled,
}

struct Job {
    id: usize,
    kind: JobKind,
    /// The database, e.g. `app_dev`.
    name: String,
    file: PathBuf,
    state: JobState,
    fraction: Option<f64>,
    detail: String,
    warnings: Vec<String>,
    cancel: CancelToken,
    _task: Option<Task<()>>,
}

impl Job {
    fn title(&self) -> String {
        let name = &self.name;
        match (self.kind, &self.state) {
            (JobKind::Dump, JobState::Running) => format!("Dumping {name}"),
            (JobKind::Dump, JobState::Finished(_)) => format!("Dumped {name}"),
            (JobKind::Dump, JobState::Failed(_)) => format!("Couldn’t dump {name}"),
            (JobKind::Dump, JobState::Cancelled) => format!("Dump of {name} stopped"),
            (JobKind::Restore, JobState::Running) => format!("Restoring into {name}"),
            (JobKind::Restore, JobState::Finished(_)) => format!("Restored into {name}"),
            (JobKind::Restore, JobState::Failed(_)) => format!("Couldn’t restore into {name}"),
            (JobKind::Restore, JobState::Cancelled) => format!("Restore into {name} stopped"),
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.state, JobState::Running)
    }

    fn update_dump(&mut self, p: &DumpProgress) {
        self.fraction = p.fraction();
        let size = format_bytes(p.bytes_written);
        self.detail = match p.phase {
            dump::DumpPhase::Connecting => "Connecting…".into(),
            dump::DumpPhase::Schema => "Writing the schema…".into(),
            dump::DumpPhase::Data => {
                let table = p.object.as_ref().map(|o| format!("{o} · ")).unwrap_or_default();
                format!("{table}{} of {} tables · {size}", p.tables_done, p.tables_total)
            }
            dump::DumpPhase::PostData => format!("Writing indexes, constraints and views… · {size}"),
            dump::DumpPhase::Finishing => "Finishing…".into(),
        };
    }

    fn update_restore(&mut self, p: &RestoreProgress) {
        self.fraction = p.fraction();
        let mut text = format!("{} of {} · {}", format_bytes(p.bytes_read), format_bytes(p.bytes_total), plural(p.statements as usize, "statement"));
        if p.errors > 0 {
            text.push_str(&format!(" · {}", plural(p.errors as usize, "error")));
        }
        self.detail = text;
    }

    fn fail(&mut self, error: dbcore::Error) {
        self.state = match error {
            dbcore::Error::Cancelled => JobState::Cancelled,
            e => JobState::Failed(e.to_string()),
        };
    }
}

/// A restore finished (or failed part way): the database's tables may have changed.
pub struct Restored {
    pub connection_id: String,
    pub database: String,
}

#[derive(Default)]
pub struct Backups {
    jobs: Vec<Job>,
    next_id: usize,
}

impl EventEmitter<Restored> for Backups {}

/// Polls a job's latest progress and its result (the work runs on the core's own runtime).
struct Watch<P, R> {
    progress: Arc<Mutex<Option<P>>>,
    result: Arc<Mutex<Option<dbcore::Result<R>>>>,
}

impl<P: Clone + Send + 'static, R: Send + 'static> Watch<P, R> {
    fn start<F>(executor: &BackgroundExecutor, work: impl FnOnce(ProgressFn<P>) -> F) -> Self
    where
        F: std::future::Future<Output = dbcore::Result<R>> + Send + 'static,
    {
        let progress = Arc::new(Mutex::new(None));
        let result = Arc::new(Mutex::new(None));
        let report: ProgressFn<P> = {
            let progress = progress.clone();
            Arc::new(move |p: &P| *progress.lock().unwrap() = Some(p.clone()))
        };
        let future = work(report);
        let done = result.clone();
        executor
            .spawn(async move {
                let r = future.await;
                *done.lock().unwrap() = Some(r);
            })
            .detach();
        Self { progress, result }
    }

    fn take(&self) -> (Option<P>, Option<dbcore::Result<R>>) {
        (self.progress.lock().unwrap().take(), self.result.lock().unwrap().take())
    }
}

impl Backups {
    fn push(&mut self, kind: JobKind, name: String, file: PathBuf, detail: String) -> (usize, CancelToken) {
        self.next_id += 1;
        let cancel = CancelToken::new();
        self.jobs.push(Job {
            id: self.next_id,
            kind,
            name,
            file,
            state: JobState::Running,
            fraction: None,
            detail,
            warnings: Vec::new(),
            cancel: cancel.clone(),
            _task: None,
        });
        (self.next_id, cancel)
    }

    fn job(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Dumps the database `target` points at (its password read here) into `path`.
    pub fn dump(
        &mut self,
        target: ConnectionConfig,
        secrets: Option<Arc<KeyringSecretStore>>,
        path: PathBuf,
        options: DumpOptions,
        cx: &mut Context<Self>,
    ) {
        let (id, cancel) = self.push(JobKind::Dump, dump::target_name(&target), path.clone(), "Connecting…".into());
        let task = cx.spawn(async move |this, cx| {
            let config = with_password(target, secrets, cx.background_executor().clone()).await;
            let watch: Watch<DumpProgress, DumpSummary> =
                Watch::start(cx.background_executor(), |report| dump::dump(config, path, options, report, cancel));
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let (progress, result) = watch.take();
                let keep = this
                    .update(cx, |backups, cx| {
                        let Some(job) = backups.job(id) else { return false };
                        if let Some(p) = progress {
                            job.update_dump(&p);
                        }
                        let finished = result.is_some();
                        match result {
                            Some(Ok(summary)) => {
                                job.fraction = Some(1.);
                                job.warnings = summary.warnings;
                                job.state = JobState::Finished(format!(
                                    "{}, {} · {}",
                                    plural(summary.tables as usize, "table"),
                                    plural(summary.rows as usize, "row"),
                                    format_bytes(summary.bytes)
                                ));
                            }
                            Some(Err(e)) => job.fail(e),
                            None => {}
                        }
                        cx.notify();
                        !finished
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        });
        if let Some(job) = self.job(id) {
            job._task = Some(task);
        }
        cx.notify();
    }

    /// Runs the script at `path` against `target` (connection `connection_id`, database `database`).
    pub fn restore(
        &mut self,
        target: ConnectionConfig,
        secrets: Option<Arc<KeyringSecretStore>>,
        path: PathBuf,
        options: RestoreOptions,
        database: String,
        cx: &mut Context<Self>,
    ) {
        let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let (id, cancel) = self.push(JobKind::Restore, dump::target_name(&target), path.clone(), file);
        let connection_id = target.id.clone();
        let task = cx.spawn(async move |this, cx| {
            let config = with_password(target, secrets, cx.background_executor().clone()).await;
            let watch: Watch<RestoreProgress, RestoreSummary> =
                Watch::start(cx.background_executor(), |report| restore::restore(config, path, options, report, cancel));
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let (progress, result) = watch.take();
                let keep = this
                    .update(cx, |backups, cx| {
                        let Some(job) = backups.job(id) else { return false };
                        if let Some(p) = progress {
                            job.update_restore(&p);
                        }
                        let finished = result.is_some();
                        match result {
                            Some(Ok(summary)) => {
                                job.fraction = Some(1.);
                                job.warnings = summary.errors.into_iter().chain(summary.warnings).collect();
                                let mut message = plural(summary.statements as usize, "statement");
                                if summary.rows > 0 {
                                    message.push_str(&format!(", {} copied", plural(summary.rows as usize, "row")));
                                }
                                if summary.error_count > 0 {
                                    message.push_str(&format!(" · {}", plural(summary.error_count as usize, "error")));
                                }
                                job.state = JobState::Finished(message);
                            }
                            Some(Err(e)) => job.fail(e),
                            None => {}
                        }
                        if finished {
                            cx.emit(Restored { connection_id: connection_id.clone(), database: database.clone() });
                        }
                        cx.notify();
                        !finished
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        });
        if let Some(job) = self.job(id) {
            job._task = Some(task);
        }
        cx.notify();
    }

    /// Stops a running job, or removes a finished one.
    fn close(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(ix) = self.jobs.iter().position(|j| j.id == id) else { return };
        if self.jobs[ix].is_running() {
            self.jobs[ix].cancel.cancel();
        } else {
            self.jobs.remove(ix);
        }
        cx.notify();
    }

    fn render_job(&self, job: &Job, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (icon, tint) = match &job.state {
            JobState::Running if job.kind == JobKind::Dump => (IconName::ArrowDown, theme.primary),
            JobState::Running => (IconName::ArrowUp, theme.primary),
            JobState::Finished(_) => (IconName::CircleCheck, theme.green),
            JobState::Failed(_) => (IconName::TriangleAlert, theme.red),
            JobState::Cancelled => (IconName::CircleX, theme.muted_foreground),
        };
        let id = job.id;
        let body = match &job.state {
            JobState::Running => v_flex()
                .gap_1()
                .child(Progress::new(("job-progress", id)).loading(job.fraction.is_none()).value(job.fraction.unwrap_or(0.) as f32 * 100.))
                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(job.detail.clone())),
            JobState::Finished(message) => v_flex()
                .gap_1()
                .child(div().text_xs().text_color(theme.muted_foreground).child(message.clone()))
                .when(!job.warnings.is_empty(), |v| {
                    let noun = if job.kind == JobKind::Dump { "warning" } else { "note" };
                    let all = SharedString::from(job.warnings.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
                    v.child(
                        div()
                            .id(("job-warnings", id))
                            .text_xs()
                            .text_color(theme.warning)
                            .child(plural(job.warnings.len(), noun))
                            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(all.clone()).build(window, cx)),
                    )
                })
                .when(job.kind == JobKind::Dump, |v| {
                    let file = job.file.clone();
                    v.child(h_flex().child(
                        Button::new(("job-reveal", id))
                            .link()
                            .xsmall()
                            .label("Show in Folder")
                            .on_click(move |_, _, cx| cx.reveal_path(&file)),
                    ))
                }),
            JobState::Failed(message) => v_flex().child(div().text_xs().text_color(theme.red).child(message.clone())),
            JobState::Cancelled => v_flex().child(div().text_xs().text_color(theme.muted_foreground).child("Cancelled")),
        };
        h_flex()
            .items_start()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .child(Icon::new(icon).text_color(tint))
            .child(v_flex().flex_1().min_w_0().gap_1().child(div().text_sm().font_semibold().truncate().child(job.title())).child(body))
            .child(
                Button::new(("job-close", id))
                    .ghost()
                    .xsmall()
                    .icon(if job.is_running() { IconName::Square } else { IconName::Close })
                    .tooltip(if job.is_running() { "Stop" } else { "Close" })
                    .on_click(cx.listener(move |this, _, _, cx| this.close(id, cx))),
            )
    }
}

impl Render for Backups {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let jobs: Vec<AnyElement> = self.jobs.iter().map(|job| self.render_job(job, cx).into_any_element()).collect();
        v_flex().w(px(340.)).gap_2().children(jobs)
    }
}
