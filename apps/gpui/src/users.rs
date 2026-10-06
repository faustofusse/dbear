//! Users & roles (Postgres, MySQL), like the macOS app: the middle column lists them (users mode),
//! the selected one shows in a tab, and dialogs create, change and drop them and their privileges.
//! The SQL comes from `dbcore::access`, and every change shows it before it runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use dbcore::access::{
    self, AccessChange, AccessFeatures, AccessStatement, DatabaseAccess, DatabaseLevel, DatabaseLevelContext, GrantObject,
    GrantObjectKind, PrivilegeSet, Role, RoleRef, RoleSpec,
};
use dbcore::{Connection, DatabaseKind, Schema};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::copy;
use crate::tabs::plural;

pub enum Load<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }
}

/// A person for logins, people for group roles.
pub fn role_icon(role: &Role) -> Icon {
    if role.can_login { Icon::new(IconName::User) } else { Icon::new(AssetIcon::Users) }
}

/// "Can log in · Superuser", "Group role (no login)"…
pub fn kind_description(role: &Role) -> String {
    let mut parts = vec![if role.can_login {
        "Can log in"
    } else if role.host.is_none() {
        "Group role (no login)"
    } else {
        "Locked account"
    }];
    if role.is_superuser {
        parts.push("Superuser");
    }
    if role.is_system {
        parts.push("Built in");
    }
    parts.join(" · ")
}

/// "Role" or "User" (MySQL accounts).
pub fn noun(kind: DatabaseKind) -> &'static str {
    if kind == DatabaseKind::Mysql { "User" } else { "Role" }
}

// MARK: - State

/// A connection's users: roles are server-wide, so one per connection. Postgres lists privileges
/// for one database, the one the middle column shows (`retarget`).
pub struct UsersState {
    pub connection: Arc<Connection>,
    /// The connection id and database it's pointed at.
    pub key: (String, String),
    pub features: AccessFeatures,
    pub roles: Load<Vec<Role>>,
    pub selected: Option<RoleRef>,
    /// The selected role's privileges, grouped by object.
    pub grants: Load<Vec<(GrantObject, PrivilegeSet)>>,
    /// Its privileges on each database of the server.
    pub access: Load<Vec<DatabaseAccess>>,
    /// Its level in databases where it has privileges (Postgres reads them in each database).
    pub levels: HashMap<String, DatabaseLevel>,
    /// Built-in roles (`pg_*`, `mysql.sys`…) are listed too.
    pub show_system: bool,
    pub search: Entity<InputState>,
    roles_task: Option<Task<()>>,
    grants_task: Option<Task<()>>,
    _search: Subscription,
}

/// A role was created: the workspace shows it.
pub struct RoleCreated(pub RoleRef);

impl EventEmitter<RoleCreated> for UsersState {}

impl UsersState {
    pub fn new(
        connection: Arc<Connection>,
        key: (String, String),
        features: AccessFeatures,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let subscription = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        let mut state = Self {
            connection,
            key,
            features,
            roles: Load::Loading,
            selected: None,
            grants: Load::Idle,
            access: Load::Idle,
            levels: HashMap::new(),
            show_system: false,
            search,
            roles_task: None,
            grants_task: None,
            _search: subscription,
        };
        state.load_roles(None, cx);
        state
    }

    pub fn kind(&self) -> DatabaseKind {
        self.connection.config().kind
    }

    /// The database the privileges are listed in (Postgres).
    pub fn database(&self) -> String {
        self.connection.config().default_database().to_string()
    }

    /// Points it at another database: Postgres privileges are per database, so they're read again.
    pub fn retarget(&mut self, connection: Arc<Connection>, key: (String, String), cx: &mut Context<Self>) {
        if key == self.key {
            return;
        }
        self.connection = connection;
        self.key = key;
        if self.features.grants_per_database {
            self.grants = if self.selected.is_some() { Load::Loading } else { Load::Idle };
            self.load_grants(cx);
        }
    }

    /// (Re)lists roles, keeping the selection (or selecting `select`), then its privileges.
    pub fn load_roles(&mut self, select: Option<RoleRef>, cx: &mut Context<Self>) {
        if self.roles.value().is_none() {
            self.roles = Load::Loading;
        }
        let connection = self.connection.clone();
        self.roles_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.list_roles().await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(roles) => {
                        let wanted = select.or(this.selected.clone());
                        this.selected = wanted.filter(|w| roles.iter().any(|r| r.reference() == *w));
                        this.roles = Load::Loaded(roles);
                    }
                    Err(e) => this.roles = Load::Failed(e.to_string()),
                }
                this.load_grants(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub fn select(&mut self, role: RoleRef, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&role) {
            return;
        }
        self.selected = Some(role);
        self.grants = Load::Loading;
        self.access = Load::Loading;
        self.load_grants(cx);
    }

    fn load_grants(&mut self, cx: &mut Context<Self>) {
        let Some(role) = self.selected.clone() else {
            self.grants = Load::Idle;
            self.access = Load::Idle;
            self.grants_task = None;
            return;
        };
        if self.grants.value().is_none() {
            self.grants = Load::Loading;
        }
        if self.access.value().is_none() {
            self.access = Load::Loading;
        }
        let connection = self.connection.clone();
        let postgres = self.kind() == DatabaseKind::Postgres;
        let shown = self.database();
        self.grants_task = Some(cx.spawn(async move |this, cx| {
            let grants = connection.list_grants(role.clone()).await;
            let access = connection.list_database_access(role.clone()).await;
            let probe: Vec<String> = match &access {
                Ok(list) if postgres => list.iter().filter(|a| needs_probe(a, &shown)).map(|a| a.database.clone()).collect(),
                _ => Vec::new(),
            };
            let same = this
                .update(cx, |this, cx| {
                    if this.selected.as_ref() != Some(&role) {
                        return false;
                    }
                    this.grants = match grants {
                        Ok(grants) => Load::Loaded(access::group_grants(&grants)),
                        Err(e) => Load::Failed(e.to_string()),
                    };
                    this.levels.clear();
                    this.access = match access {
                        Ok(list) => Load::Loaded(list),
                        Err(e) => Load::Failed(e.to_string()),
                    };
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !same {
                return;
            }
            // Postgres: what the role has inside each database it has privileges on.
            for database in probe {
                let level = connection.database_level(role.clone(), database.clone()).await.ok().map(|c| c.level);
                let keep = this
                    .update(cx, |this, cx| {
                        if this.selected.as_ref() != Some(&role) {
                            return false;
                        }
                        if let Some(level) = level {
                            this.levels.insert(database, level);
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
        cx.notify();
    }

    pub fn selected_role(&self) -> Option<&Role> {
        let selected = self.selected.as_ref()?;
        self.roles.value()?.iter().find(|r| r.reference() == *selected)
    }

    /// The list as shown: built-in roles only when asked for (or selected), filtered by the search.
    pub fn visible_roles(&self, cx: &App) -> Vec<Role> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        self.roles
            .value()
            .map(|roles| {
                roles
                    .iter()
                    .filter(|r| self.show_system || !r.is_system || Some(r.reference()) == self.selected)
                    .filter(|r| query.is_empty() || r.reference().title().to_lowercase().contains(&query))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Roles that are members of `role` (inherit its privileges).
    pub fn members(&self, role: &Role) -> Vec<Role> {
        let reference = role.reference();
        self.roles.value().map(|r| r.iter().filter(|m| m.member_of.contains(&reference)).cloned().collect()).unwrap_or_default()
    }

    /// The role a spec names: trimmed, MySQL hosts defaulting to `%`.
    pub fn reference_for(&self, spec: &RoleSpec) -> RoleRef {
        let host = spec.host.as_deref().map(str::trim).filter(|h| !h.is_empty()).unwrap_or("%");
        RoleRef::new(spec.name.trim(), self.features.hosts.then(|| host.to_string()))
    }

    /// Privileges the selected role holds on `object`, as listed.
    pub fn privileges_on(&self, object: &GrantObject) -> PrivilegeSet {
        self.grants.value().and_then(|g| g.iter().find(|(o, _)| o == object)).map(|(_, p)| p.clone()).unwrap_or_default()
    }

    pub fn preview(&self, changes: &[AccessChange]) -> Result<Vec<AccessStatement>, String> {
        self.connection.preview_access(changes).map_err(|e| e.to_string())
    }

    /// Runs `changes` (in one transaction where possible), then lists the roles again, selecting
    /// the one changed.
    pub fn apply(&mut self, changes: Vec<AccessChange>, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let mut select = None;
        let mut dropped = false;
        let mut created = None;
        for change in &changes {
            match change {
                AccessChange::CreateRole(spec) => {
                    select = Some(self.reference_for(spec));
                    created = select.clone();
                }
                AccessChange::AlterRole { spec, .. } => select = Some(self.reference_for(spec)),
                AccessChange::DropRole(_) => dropped = true,
                AccessChange::SetPrivileges { role, .. } | AccessChange::SetDatabaseLevel { role, .. } => {
                    select = select.or_else(|| Some(role.clone()))
                }
            }
        }
        let connection = self.connection.clone();
        cx.spawn(async move |this, cx| {
            connection.apply_access(changes).await.map_err(|e| e.to_string())?;
            this.update(cx, |this, cx| {
                if dropped && select.is_none() {
                    this.selected = None;
                }
                // Read again: what was applied is what the server now says.
                this.grants = Load::Loading;
                this.access = Load::Loading;
                this.load_roles(select, cx);
                if let Some(role) = created {
                    cx.emit(RoleCreated(role));
                }
            })
            .ok();
            Ok(())
        })
    }
}

/// Whether a Postgres database's level must be read in it. The listing only sees privileges on the
/// database itself, so a role with none there still may have some inside: the database shown (whose
/// privileges are listed) is always read.
fn needs_probe(access: &DatabaseAccess, shown: &str) -> bool {
    !access.is_owner && (access.level == DatabaseLevel::Custom || access.database == shown)
}

// MARK: - Confirmations

fn show_error(message: String, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| alert.title("Couldn’t Apply the Change").description(message.clone()));
}

/// Runs a change confirmed in an alert; a failure shows in another alert.
fn run_confirmed(state: &Entity<UsersState>, change: AccessChange, window: &mut Window, cx: &mut App) {
    let task = state.update(cx, |s, cx| s.apply(vec![change], cx));
    window
        .spawn(cx, async move |cx| {
            if let Err(e) = task.await {
                cx.update(|window, cx| show_error(e, window, cx)).ok();
            }
        })
        .detach();
}

pub fn confirm_drop(state: Entity<UsersState>, role: Role, window: &mut Window, cx: &mut App) {
    let title = role.reference().title();
    let postgres = state.read(cx).kind() == DatabaseKind::Postgres;
    let description = if postgres {
        "It can’t be dropped while it owns objects or holds privileges in any database: reassign or drop those first."
    } else {
        "The account and its privileges are removed. Connections already open stay open."
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, role) = (state.clone(), role.reference());
        alert
            .title(format!("Drop “{title}”?"))
            .description(description)
            .show_cancel(true)
            .button_props(DialogButtonProps::default().ok_text("Drop"))
            .on_ok(move |_, window, cx| {
                run_confirmed(&state, AccessChange::DropRole(role.clone()), window, cx);
                true
            })
    });
}

pub fn confirm_revoke(state: Entity<UsersState>, role: RoleRef, object: GrantObject, before: PrivilegeSet, window: &mut Window, cx: &mut App) {
    let message = format!("{} loses {} on {}.", role.title(), before.privileges.join(", "), object.title());
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, role, object, before) = (state.clone(), role.clone(), object.clone(), before.clone());
        alert
            .title("Revoke These Privileges?")
            .description(message.clone())
            .show_cancel(true)
            .button_props(DialogButtonProps::default().ok_text("Revoke"))
            .on_ok(move |_, window, cx| {
                let change = AccessChange::SetPrivileges { role: role.clone(), object: object.clone(), before: before.clone(), after: PrivilegeSet::default() };
                run_confirmed(&state, change, window, cx);
                true
            })
    });
}

// MARK: - Tab

/// The selected role: attributes, memberships, database access and privileges.
pub struct UsersTab {
    pub state: Entity<UsersState>,
    _observe: Subscription,
}

impl UsersTab {
    pub fn new(state: Entity<UsersState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self { state, _observe: observe }
    }

    pub fn title(&self, cx: &App) -> String {
        let state = self.state.read(cx);
        match &state.selected {
            Some(role) => role.title(),
            None => if state.kind() == DatabaseKind::Mysql { "Users" } else { "Roles" }.to_string(),
        }
    }
}

fn section(title: &str, count: Option<usize>, cx: &App) -> Div {
    v_flex().gap_2().child(
        h_flex()
            .gap_2()
            .items_baseline()
            .child(div().font_semibold().child(title.to_string()))
            .children(count.map(|n| div().text_sm().text_color(cx.theme().muted_foreground).child(n.to_string()))),
    )
}

fn info_row(label: &str, value: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .text_sm()
        .child(div().w(px(160.)).flex_shrink_0().text_color(cx.theme().muted_foreground).child(label.to_string()))
        .child(div().flex_1().min_w_0().child(value))
}

fn yes_no(value: bool, cx: &App) -> impl IntoElement {
    if value {
        h_flex().gap_1().child(Icon::new(IconName::Check).xsmall()).child("Yes").into_any_element()
    } else {
        div().text_color(cx.theme().muted_foreground).child("No").into_any_element()
    }
}

impl Render for UsersTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let theme = cx.theme();
        let Some(role) = state.selected_role().cloned() else {
            let message = match &state.roles {
                Load::Failed(e) => e.clone(),
                Load::Loading | Load::Idle => String::new(),
                Load::Loaded(_) => format!("Choose a {} in the list.", noun(state.kind()).to_lowercase()),
            };
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(message)
                .into_any_element();
        };
        let features = state.features.clone();
        let kind = state.kind();
        let reference = role.reference();
        let entity = self.state.clone();

        // Header.
        let header = h_flex()
            .gap_3()
            .child(
                div()
                    .size(px(44.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if role.is_superuser { theme.warning } else { theme.primary })
                    .text_color(theme.primary_foreground)
                    .child(role_icon(&role)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_xl().font_semibold().truncate().child(reference.title()))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(kind_description(&role))),
            )
            .child(Button::new("edit-role").outline().small().label("Edit…").on_click({
                let (state, role) = (entity.clone(), role.clone());
                move |_, window, cx| open_role_editor(state.clone(), Some(role.clone()), window, cx)
            }))
            .child(Button::new("drop-role").ghost().small().label("Drop…").disabled(role.is_system).on_click({
                let (state, role) = (entity.clone(), role.clone());
                move |_, window, cx| confirm_drop(state.clone(), role.clone(), window, cx)
            }));

        // Attributes.
        let mut attributes = section("Attributes", None, cx).child(info_row("Can log in", yes_no(role.can_login, cx), cx));
        if features.superuser {
            attributes = attributes.child(info_row("Superuser", yes_no(role.is_superuser, cx), cx));
        } else if role.is_superuser {
            attributes = attributes.child(info_row("SUPER privilege", yes_no(true, cx), cx));
        }
        if features.create_db {
            attributes = attributes.child(info_row("Create databases", yes_no(role.can_create_db, cx), cx));
        }
        if features.create_role {
            attributes = attributes.child(info_row("Create roles", yes_no(role.can_create_role, cx), cx));
        }
        if features.connection_limit {
            let limit = role.connection_limit.map_or("Unlimited".to_string(), |n| n.to_string());
            attributes = attributes.child(info_row("Connection limit", limit, cx));
        }
        if features.valid_until {
            attributes = attributes.child(info_row("Password expires", role.valid_until.clone().unwrap_or("Never".into()), cx));
        }
        if let Some(comment) = role.comment.clone().filter(|c| !c.is_empty()) {
            attributes = attributes.child(info_row("Comment", comment, cx));
        }

        // Memberships.
        let chips = |roles: Vec<RoleRef>, id: &'static str| {
            let mut row = h_flex().flex_wrap().gap_1();
            for (i, r) in roles.into_iter().enumerate() {
                let state = entity.clone();
                row = row.child(
                    Button::new((id, i)).outline().xsmall().label(r.title()).on_click(move |_, _, cx| {
                        let r = r.clone();
                        state.update(cx, |s, cx| s.select(r, cx));
                    }),
                );
            }
            row
        };
        let members: Vec<RoleRef> = state.members(&role).iter().map(Role::reference).collect();
        let membership = features.membership.then(|| {
            let mut v = v_flex().gap_6().child(section("Member Of", Some(role.member_of.len()), cx).child(if role.member_of.is_empty() {
                div().text_sm().text_color(theme.muted_foreground).child("Not a member of any role").into_any_element()
            } else {
                chips(role.member_of.clone(), "member-of").into_any_element()
            }));
            if !members.is_empty() {
                v = v.child(section("Members", Some(members.len()), cx).child(chips(members.clone(), "member")));
            }
            v
        });

        // Database access.
        let access = match &state.access {
            Load::Idle | Load::Loading => section("Database Access", None, cx).child(Spinner::new().small()),
            Load::Failed(e) => section("Database Access", None, cx).child(div().text_sm().text_color(theme.muted_foreground).child(e.clone())),
            Load::Loaded(list) => {
                // With privileges on the database itself, or inside it (as read there).
                let probed = |a: &DatabaseAccess| state.levels.get(&a.database).is_some_and(|l| *l != DatabaseLevel::NoAccess);
                let granted: Vec<&DatabaseAccess> =
                    list.iter().filter(|a| a.is_owner || !a.privileges.privileges.is_empty() || probed(a)).collect();
                let mut s = section("Database Access", Some(granted.len()), cx);
                if granted.is_empty() {
                    s = s.child(div().text_sm().text_color(theme.muted_foreground).child("No privileges granted on any database directly."));
                }
                for a in granted {
                    let level = state.levels.get(&a.database).copied().unwrap_or(a.level);
                    let text = if a.is_owner {
                        "Owner".to_string()
                    } else if level != DatabaseLevel::Custom {
                        level.title().to_string()
                    } else {
                        let grant = if a.privileges.grantable { " (with grant option)" } else { "" };
                        if a.privileges.privileges.is_empty() {
                            "Custom: privileges inside the database that match no level".to_string()
                        } else {
                            format!("Custom: {} on the database{grant}", a.privileges.privileges.join(", "))
                        }
                    };
                    s = s.child(info_row(&a.database, text, cx));
                }
                let open: Vec<&str> = list.iter().filter(|a| a.everyone_can_connect).map(|a| a.database.as_str()).collect();
                if !open.is_empty() {
                    s = s.child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                        "Any role can connect to {} (granted to PUBLIC).",
                        open.join(", ")
                    )));
                }
                s
            }
        };

        // Privileges.
        let mut privileges = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(div().font_semibold().child("Privileges"))
                .children(state.grants.value().map(|g| div().text_sm().text_color(theme.muted_foreground).child(g.len().to_string())))
                .when(features.grants_per_database, |h| {
                    h.child(div().text_sm().text_color(theme.muted_foreground).child(format!("in {}", state.database())))
                })
                .child(div().flex_1())
                .child(Button::new("grant").outline().small().label("Grant…").on_click({
                    let (state, role) = (entity.clone(), reference.clone());
                    move |_, window, cx| open_privilege_editor(state.clone(), role.clone(), None, window, cx)
                })),
        );
        if role.is_superuser && kind == DatabaseKind::Postgres {
            privileges = privileges.child(div().text_sm().text_color(theme.muted_foreground).child("Superusers bypass all privilege checks."));
        }
        privileges = match &state.grants {
            Load::Idle | Load::Loading => privileges.child(Spinner::new().small()),
            Load::Failed(e) => privileges.child(div().text_sm().text_color(theme.muted_foreground).child(e.clone())),
            Load::Loaded(grants) if grants.is_empty() => {
                let inherited = if role.member_of.is_empty() { "" } else { " It may still inherit privileges from the roles it’s a member of." };
                privileges.child(div().text_sm().text_color(theme.muted_foreground).child(format!("No privileges granted directly.{inherited}")))
            }
            Load::Loaded(grants) => {
                let mut table = v_flex().rounded_md().border_1().border_color(theme.border).overflow_hidden().child(
                    h_flex()
                        .bg(theme.table_head)
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(div().w(px(200.)).px_2().py_1().child("Object"))
                        .child(div().flex_1().px_2().py_1().child("Privileges")),
                );
                for (i, (object, set)) in grants.iter().enumerate() {
                    let grantable = if set.grantable { " · with grant option" } else { "" };
                    table = table.child(
                        h_flex()
                            .items_start()
                            .text_sm()
                            .border_t_1()
                            .border_color(theme.border)
                            .when(i % 2 == 1, |r| r.bg(theme.table_even))
                            .child(
                                v_flex()
                                    .w(px(200.))
                                    .px_2()
                                    .py_1()
                                    .child(div().truncate().child(object.title()))
                                    .child(div().text_xs().text_color(theme.muted_foreground).child(object.kind().title())),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .px_2()
                                    .py_1()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .child(format!("{}{grantable}", set.privileges.join(", "))),
                            )
                            .child(
                                h_flex()
                                    .px_1()
                                    .gap_1()
                                    .child(Button::new(("edit-grant", i)).ghost().xsmall().label("Edit").on_click({
                                        let (state, role, object) = (entity.clone(), reference.clone(), object.clone());
                                        move |_, window, cx| open_privilege_editor(state.clone(), role.clone(), Some(object.clone()), window, cx)
                                    }))
                                    .child(Button::new(("revoke-grant", i)).ghost().xsmall().label("Revoke").on_click({
                                        let (state, role, object, set) = (entity.clone(), reference.clone(), object.clone(), set.clone());
                                        move |_, window, cx| confirm_revoke(state.clone(), role.clone(), object.clone(), set.clone(), window, cx)
                                    })),
                            ),
                    );
                }
                privileges.child(table)
            }
        };

        div()
            .id("users-tab")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .gap_6()
                    .px_5()
                    .py_4()
                    .child(header)
                    .child(attributes)
                    .children(membership)
                    .when(matches!(kind, DatabaseKind::Postgres | DatabaseKind::Mysql), |v| v.child(access))
                    .child(privileges),
            )
            .into_any_element()
    }
}

// MARK: - Role editor

pub struct RoleEditor {
    state: Entity<UsersState>,
    original: Option<Role>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    password: Entity<InputState>,
    limit: Entity<InputState>,
    valid_until: Entity<InputState>,
    can_login: bool,
    superuser: bool,
    create_db: bool,
    create_role: bool,
    member_of: Vec<RoleRef>,
    show_builtin: bool,
    /// Privileges on each database as listed, the level picked per database, and each one's
    /// current level (read in that database).
    access: Load<Vec<DatabaseAccess>>,
    wanted: BTreeMap<String, DatabaseLevel>,
    contexts: HashMap<String, DatabaseLevelContext>,
    probe_errors: HashMap<String, String>,
    /// Databases whose level was asked for in them.
    probed: std::collections::HashSet<String>,
    /// The role levels are read for: a new role has none, so a name nobody has.
    probe_role: RoleRef,
    pub saving: bool,
    error: Option<String>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RoleEditor {
    pub fn new(state: Entity<UsersState>, original: Option<Role>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (kind, hosts) = {
            let s = state.read(cx);
            (s.kind(), s.features.hosts)
        };
        let input = |value: String, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let o = original.as_ref();
        let name = input(o.map(|r| r.name.clone()).unwrap_or_default(), if kind == DatabaseKind::Mysql { "app_user" } else { "app_reader" }, window, cx);
        let host = input(o.and_then(|r| r.host.clone()).unwrap_or_else(|| if hosts { "%".into() } else { String::new() }), "%", window, cx);
        let password_placeholder = if original.is_some() { "Unchanged" } else { "None" };
        let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(password_placeholder));
        let limit = input(o.and_then(|r| r.connection_limit).map(|n| n.to_string()).unwrap_or_default(), "Unlimited", window, cx);
        let valid_until = input(o.and_then(|r| r.valid_until.clone()).unwrap_or_default(), "Never (e.g. 2026-12-31)", window, cx);
        let subscriptions = [&name, &host, &password, &limit, &valid_until]
            .into_iter()
            .map(|i| {
                cx.subscribe(i, |this: &mut Self, _, _: &InputEvent, cx| {
                    this.error = None;
                    cx.notify()
                })
            })
            .collect();
        let probe_role = o.map(Role::reference).unwrap_or_else(|| RoleRef::new(format!("dbear-new-{}", std::process::id()), None));
        name.update(cx, |i, cx| i.focus(window, cx));
        let mut editor = Self {
            state,
            can_login: o.is_none_or(|r| r.can_login),
            superuser: o.is_some_and(|r| r.is_superuser),
            create_db: o.is_some_and(|r| r.can_create_db),
            create_role: o.is_some_and(|r| r.can_create_role),
            member_of: o.map(|r| r.member_of.clone()).unwrap_or_default(),
            original,
            name,
            host,
            password,
            limit,
            valid_until,
            show_builtin: false,
            access: Load::Loading,
            wanted: BTreeMap::new(),
            contexts: HashMap::new(),
            probe_errors: HashMap::new(),
            probed: std::collections::HashSet::new(),
            probe_role,
            saving: false,
            error: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        editor.load_access(cx);
        editor
    }

    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }

    fn kind(&self, cx: &App) -> DatabaseKind {
        self.state.read(cx).kind()
    }

    fn spec(&self, cx: &App) -> RoleSpec {
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
        let features = &self.state.read(cx).features;
        let password = self.password.read(cx).value().to_string();
        RoleSpec {
            name: text(&self.name),
            host: features.hosts.then(|| text(&self.host)),
            password: (!password.is_empty()).then_some(password),
            can_login: self.can_login,
            is_superuser: features.superuser && self.superuser,
            can_create_db: features.create_db && self.create_db,
            can_create_role: features.create_role && self.create_role,
            connection_limit: text(&self.limit).parse().ok(),
            valid_until: Some(text(&self.valid_until)).filter(|v| features.valid_until && !v.is_empty()),
            member_of: self.member_of.clone(),
        }
    }

    /// The role, then the database levels: granted to the role's new name.
    fn changes(&self, cx: &App) -> Vec<AccessChange> {
        let spec = self.spec(cx);
        let role = self.state.read(cx).reference_for(&spec);
        let mut changes = vec![match &self.original {
            Some(original) => AccessChange::AlterRole { role: original.clone(), spec },
            None => AccessChange::CreateRole(spec),
        }];
        for (database, level) in &self.wanted {
            if let Some(context) = self.contexts.get(database).filter(|c| c.level != *level) {
                changes.push(AccessChange::SetDatabaseLevel { role: role.clone(), context: context.clone(), level: *level });
            }
        }
        changes
    }

    /// Databases whose new level is picked but whose current one is still being read.
    fn pending(&self) -> Vec<String> {
        self.wanted.keys().filter(|d| !self.contexts.contains_key(*d)).cloned().collect()
    }

    pub fn can_save(&self, cx: &App) -> bool {
        !self.saving && self.pending().is_empty() && self.state.read(cx).preview(&self.changes(cx)).is_ok_and(|s| !s.is_empty())
    }

    /// The statements, with a comment before those that run in another database (Postgres levels).
    fn preview_text(&self, cx: &App) -> Result<String, String> {
        let state = self.state.read(cx);
        let changes = self.changes(cx);
        state.preview(&changes)?;
        let mut lines: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        for change in &changes {
            let statements = state.preview(std::slice::from_ref(change))?;
            if statements.is_empty() {
                continue;
            }
            let runs_in = change.database(state.kind()).map(str::to_string);
            if runs_in != current {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(format!("-- in {}", runs_in.clone().unwrap_or_else(|| state.database())));
                current = runs_in;
            }
            lines.extend(statements.into_iter().map(|s| format!("{};", s.display)));
        }
        Ok(lines.join("\n"))
    }

    fn load_access(&mut self, cx: &mut Context<Self>) {
        let connection = self.state.read(cx).connection.clone();
        let (role, mysql) = (self.probe_role.clone(), self.kind(cx) == DatabaseKind::Mysql);
        let shown = if self.original.is_some() { self.state.read(cx).database() } else { String::new() };
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = connection.list_database_access(role).await;
            let probe = this
                .update(cx, |this, cx| {
                    let mut probe = Vec::new();
                    match result {
                        Ok(databases) => {
                            for d in &databases {
                                if mysql {
                                    // `db.*` privileges tell the level: nothing to read elsewhere.
                                    this.contexts.insert(
                                        d.database.clone(),
                                        DatabaseLevelContext {
                                            database: d.database.clone(),
                                            level: d.level,
                                            privileges: d.privileges.clone(),
                                            schemas: Vec::new(),
                                            owners: Vec::new(),
                                        },
                                    );
                                } else if needs_probe(d, &shown) {
                                    probe.push(d.database.clone());
                                }
                            }
                            this.access = Load::Loaded(databases);
                        }
                        Err(e) => this.access = Load::Failed(e.to_string()),
                    }
                    cx.notify();
                    probe
                })
                .unwrap_or_default();
            for database in probe {
                this.update(cx, |this, cx| this.probe(database, cx)).ok();
            }
        }));
    }

    fn probe(&mut self, database: String, cx: &mut Context<Self>) {
        self.probe_errors.remove(&database);
        self.probed.insert(database.clone());
        let connection = self.state.read(cx).connection.clone();
        let role = self.probe_role.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = connection.database_level(role, database.clone()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(context) => {
                        this.contexts.insert(database, context);
                    }
                    Err(e) => {
                        this.probe_errors.insert(database, e.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Its level is being read in that database (or the reading failed).
    fn probing(&self, database: &str) -> bool {
        self.probed.contains(database)
    }

    /// Picks a level; its current one must be known first (read in that database).
    fn pick(&mut self, database: String, level: DatabaseLevel, cx: &mut Context<Self>) {
        let current = self.contexts.get(&database).map(|c| c.level);
        if level == DatabaseLevel::Custom || Some(level) == current {
            self.wanted.remove(&database);
        } else {
            self.wanted.insert(database.clone(), level);
            if current.is_none() {
                self.probe(database, cx);
            }
        }
        self.error = None;
        cx.notify();
    }

    fn generate_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match access::generate_password(24) {
            Ok(password) => {
                self.password.update(cx, |i, cx| {
                    i.set_value(password, window, cx);
                    i.set_masked(false, window, cx);
                });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        cx.notify();
    }

    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            return;
        }
        self.saving = true;
        self.error = None;
        let changes = self.changes(cx);
        let task = self.state.update(cx, |s, cx| s.apply(changes, cx));
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => window.close_dialog(cx),
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_access(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let kind = self.kind(cx);
        let list = match &self.access {
            Load::Idle | Load::Loading => return Spinner::new().small().into_any_element(),
            Load::Failed(e) => return div().text_sm().text_color(theme.muted_foreground).child(e.clone()).into_any_element(),
            Load::Loaded(list) => list,
        };
        let levels = access::database_levels(kind);
        let mut rows = v_flex().gap_1();
        for (i, database) in list.iter().enumerate() {
            let name = database.database.clone();
            let current = self.contexts.get(&name).map(|c| c.level);
            let shown = self.wanted.get(&name).copied().or(current).unwrap_or(database.level);
            let control = if database.is_owner {
                div().text_sm().text_color(theme.muted_foreground).child("Owner").into_any_element()
            } else if current.is_none() && self.probing(&name) && !self.wanted.contains_key(&name) {
                // It has privileges there: its level is being read in that database.
                match self.probe_errors.get(&name) {
                    Some(e) => div().text_xs().text_color(theme.red).child(e.clone()).into_any_element(),
                    None => Spinner::new().small().into_any_element(),
                }
            } else {
                let this = cx.entity().downgrade();
                let (levels, custom) = (levels.clone(), shown == DatabaseLevel::Custom || current == Some(DatabaseLevel::Custom));
                let db = name.clone();
                Button::new(("level", i))
                    .outline()
                    .small()
                    .label(if shown == DatabaseLevel::Custom { "Custom (unchanged)" } else { shown.title() })
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _, _| {
                        for level in levels.iter().copied() {
                            let (this, db) = (this.clone(), db.clone());
                            menu = menu.item(PopupMenuItem::new(level.title()).checked(level == shown).on_click(move |_, _, cx| {
                                let db = db.clone();
                                this.update(cx, |e, cx| e.pick(db, level, cx)).ok();
                            }));
                        }
                        if custom {
                            let (this, db) = (this.clone(), db.clone());
                            menu = menu.separator().item(PopupMenuItem::new("Custom (unchanged)").checked(shown == DatabaseLevel::Custom).on_click(
                                move |_, _, cx| {
                                    let db = db.clone();
                                    this.update(cx, |e, cx| e.pick(db, DatabaseLevel::Custom, cx)).ok();
                                },
                            ));
                        }
                        menu
                    })
                    .into_any_element()
            };
            let note = if let Some(e) = self.probe_errors.get(&name).filter(|_| self.wanted.contains_key(&name)) {
                Some((e.clone(), theme.red))
            } else if !database.is_owner && shown != DatabaseLevel::NoAccess {
                let text = if shown == DatabaseLevel::Custom {
                    "Privileges that match no level.".to_string()
                } else {
                    shown.summary(kind).to_string()
                };
                Some((text, theme.muted_foreground))
            } else if database.everyone_can_connect {
                Some(("Anyone can connect (granted to PUBLIC), but not read tables.".to_string(), theme.muted_foreground))
            } else {
                None
            };
            rows = rows.child(
                h_flex()
                    .gap_3()
                    .py_1()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(name))
                            .children(note.map(|(t, c)| div().text_xs().text_color(c).child(t))),
                    )
                    .child(control),
            );
        }
        rows.into_any_element()
    }
}

fn form_row(label: &str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .child(div().w(px(130.)).flex_shrink_0().text_sm().text_color(cx.theme().muted_foreground).child(label.to_string()))
        .child(div().flex_1().min_w_0().child(control))
}

fn group(title: &str, cx: &App) -> Div {
    v_flex().gap_2().child(div().text_xs().font_semibold().text_color(cx.theme().muted_foreground).child(title.to_uppercase()))
}

fn sql_box(text: String, cx: &App) -> impl IntoElement {
    div()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .child(text)
}

impl Render for RoleEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (features, kind, candidates) = {
            let s = self.state.read(cx);
            let me = self.original.as_ref().map(Role::reference);
            let candidates: Vec<Role> = s.roles.value().map(|r| r.iter().filter(|x| Some(x.reference()) != me).cloned().collect()).unwrap_or_default();
            (s.features.clone(), s.kind(), candidates)
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;

        let mut identity = group("Identity", cx).child(form_row("Name", Input::new(&self.name).small(), cx));
        if features.hosts {
            identity = identity
                .child(form_row("Host", Input::new(&self.host).small(), cx))
                .child(div().text_xs().text_color(muted).child(
                    "Where the user may connect from: % for anywhere, localhost, or an address pattern like 10.0.%.",
                ));
        }
        identity = identity.child(form_row(
            "Password",
            h_flex()
                .gap_2()
                .child(div().flex_1().child(Input::new(&self.password).small().mask_toggle()))
                .child(Button::new("generate").outline().small().label("Generate").on_click(cx.listener(|this, _, window, cx| this.generate_password(window, cx)))),
            cx,
        ));
        if !self.is_new() && kind == DatabaseKind::Postgres {
            identity = identity.child(div().text_xs().text_color(muted).child("Renaming a role clears its MD5 password; set a new one."));
        }

        let check = |id: &'static str, label: &'static str, value: bool, set: fn(&mut Self, bool), cx: &mut Context<Self>| {
            Checkbox::new(id).checked(value).label(label).on_click(cx.listener(move |this, checked: &bool, _, cx| {
                set(this, *checked);
                this.error = None;
                cx.notify();
            }))
        };
        let mut permissions = group("Permissions", cx).child(check("can-login", "Can log in", self.can_login, |e, v| e.can_login = v, cx));
        permissions = permissions.child(div().text_xs().text_color(muted).child(if features.hosts {
            "Unchecked, the account is locked (a role for others to be granted)."
        } else {
            "Unchecked, it’s a group role others can be members of."
        }));
        if features.superuser {
            permissions = permissions.child(check("superuser", "Superuser (bypasses every permission check)", self.superuser, |e, v| e.superuser = v, cx));
        }
        if features.create_db {
            permissions = permissions.child(check("create-db", "Create databases", self.create_db, |e, v| e.create_db = v, cx));
        }
        if features.create_role {
            permissions = permissions.child(check("create-role", "Create roles", self.create_role, |e, v| e.create_role = v, cx));
        }

        let limits = (features.connection_limit || features.valid_until).then(|| {
            let mut g = group("Limits", cx);
            if features.connection_limit {
                g = g.child(form_row("Connection limit", Input::new(&self.limit).small(), cx));
            }
            if features.valid_until {
                g = g.child(form_row("Password expires", Input::new(&self.valid_until).small(), cx));
            }
            g
        });

        let membership = features.membership.then(|| {
            let mut g = group("Member Of", cx);
            let toggle = |role: &Role, cx: &mut Context<Self>| {
                let reference = role.reference();
                let checked = self.member_of.contains(&reference);
                Checkbox::new(SharedString::from(format!("member-{}", reference.title())))
                    .checked(checked)
                    .label(reference.title())
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        this.member_of.retain(|r| *r != reference);
                        if *on {
                            this.member_of.push(reference.clone());
                        }
                        this.error = None;
                        cx.notify();
                    }))
            };
            let (regular, builtin): (Vec<&Role>, Vec<&Role>) = candidates.iter().partition(|r| !r.is_system);
            if candidates.is_empty() {
                g = g.child(div().text_sm().text_color(muted).child("There are no other roles."));
            }
            for role in regular {
                g = g.child(toggle(role, cx));
            }
            if !builtin.is_empty() {
                let shown = self.show_builtin;
                g = g.child(h_flex().child(
                    Button::new("builtin-roles")
                        .ghost()
                        .xsmall()
                        .icon(if shown { IconName::ChevronDown } else { IconName::ChevronRight })
                        .label(format!("Built-in Roles ({})", builtin.len()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_builtin = !this.show_builtin;
                            cx.notify();
                        })),
                ));
                if shown {
                    for role in builtin {
                        g = g.child(div().pl_4().child(toggle(role, cx)));
                    }
                }
            }
            g.child(div().text_xs().text_color(muted).child("Members inherit the privileges of the roles they belong to."))
        });

        let footer_note = if kind == DatabaseKind::Postgres {
            let base = "Levels cover every schema in the database, and tables, sequences and schemas created later. Picking one replaces what the role had there.";
            if self.superuser { format!("Superusers can do anything in every database. {base}") } else { base.to_string() }
        } else {
            "Privileges on every table of the database (database.*), including ones created later.".to_string()
        };
        let access = group("Database Access", cx).child(self.render_access(cx)).child(div().text_xs().text_color(muted).child(footer_note));

        let pending = self.pending();
        let mut sql = group("SQL", cx);
        if !pending.is_empty() {
            sql = sql.child(div().text_xs().text_color(muted).child(format!("Reading access in {}…", pending.join(", "))));
        }
        sql = match self.preview_text(cx) {
            Ok(text) if text.is_empty() => sql.child(div().text_sm().text_color(muted).child("Nothing changed.")),
            Ok(text) => sql.child(sql_box(text, cx)),
            Err(e) => sql.child(div().text_sm().text_color(muted).child(e)),
        };

        v_flex()
            .gap_2()
            .child(
                div().id("role-editor").h(px(500.)).overflow_y_scrollbar().child(
                    v_flex()
                        .gap_5()
                        .pr_3()
                        .child(identity)
                        .child(permissions)
                        .children(limits)
                        .children(membership)
                        .child(access)
                        .child(sql),
                ),
            )
            .children(self.error.clone().map(|e| div().text_sm().text_color(cx.theme().red).child(e)))
    }
}

pub fn open_role_editor(state: Entity<UsersState>, original: Option<Role>, window: &mut Window, cx: &mut App) {
    let kind = state.read(cx).kind();
    let title = match &original {
        Some(role) => format!("Edit {}", role.reference().title()),
        None => format!("New {}", noun(kind)),
    };
    let editor = cx.new(|cx| RoleEditor::new(state, original, window, cx));
    // The dialog takes focus as it opens: give it to the name once it's there.
    let name = editor.read(cx).name.clone();
    window.on_next_frame(move |window, cx| name.update(cx, |i, cx| i.focus(window, cx)));
    window.open_dialog(cx, move |modal, _, cx| {
        let (saving, ready, new) = {
            let e = editor.read(cx);
            (e.saving, e.can_save(cx), e.is_new())
        };
        let footer = h_flex()
            .w_full()
            .gap_2()
            .when(saving, |f| f.child(Spinner::new().small()))
            .child(div().flex_1())
            .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
            .child(Button::new("save").primary().label(if new { "Create" } else { "Save" }).disabled(!ready).on_click({
                let editor = editor.clone();
                move |_, window, cx| editor.update(cx, |e, cx| e.save(window, cx))
            }));
        modal.title(title.clone()).w(px(580.)).child(editor.clone()).footer(footer)
    });
}

// MARK: - Privilege editor

pub struct PrivilegeEditor {
    state: Entity<UsersState>,
    role: RoleRef,
    /// Editing a listed grant: the object is fixed.
    fixed: Option<GrantObject>,
    kind: GrantObjectKind,
    database: String,
    schema: String,
    table: String,
    after: PrivilegeSet,
    /// Schemas (MySQL: databases) and their tables, and the server's databases (MySQL), to pick from.
    schemas: Vec<Schema>,
    databases: Vec<String>,
    loading: bool,
    pub saving: bool,
    error: Option<String>,
    _tasks: Vec<Task<()>>,
}

impl PrivilegeEditor {
    pub fn new(state: Entity<UsersState>, role: RoleRef, fixed: Option<GrantObject>, cx: &mut Context<Self>) -> Self {
        let kinds = state.read(cx).features.object_kinds.clone();
        let fallback = if kinds.contains(&GrantObjectKind::Table) { GrantObjectKind::Table } else { kinds.first().copied().unwrap_or(GrantObjectKind::Table) };
        let (mut database, mut schema, mut table) = (String::new(), String::new(), String::new());
        match &fixed {
            Some(GrantObject::Database { name }) => database = name.clone(),
            Some(GrantObject::Schema { name } | GrantObject::AllTables { schema: name } | GrantObject::AllSequences { schema: name }) => {
                schema = name.clone()
            }
            Some(GrantObject::Table { schema: s, name } | GrantObject::Sequence { schema: s, name }) => {
                schema = s.clone();
                table = name.clone();
            }
            _ => {}
        }
        let mut editor = Self {
            kind: fixed.as_ref().map_or(fallback, GrantObject::kind),
            state,
            role,
            fixed,
            database,
            schema,
            table,
            after: PrivilegeSet::default(),
            schemas: Vec::new(),
            databases: Vec::new(),
            loading: true,
            saving: false,
            error: None,
            _tasks: Vec::new(),
        };
        editor.load(cx);
        editor
    }

    fn db_kind(&self, cx: &App) -> DatabaseKind {
        self.state.read(cx).kind()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let connection = self.state.read(cx).connection.clone();
        let mysql = self.db_kind(cx) == DatabaseKind::Mysql;
        self._tasks.push(cx.spawn(async move |this, cx| {
            let schemas = connection.list_schemas().await;
            let databases = if mysql { connection.list_databases().await } else { Ok(Vec::new()) };
            this.update(cx, |this, cx| {
                match (schemas, databases) {
                    (Ok(schemas), Ok(databases)) => {
                        this.schemas = schemas;
                        this.databases = databases;
                        if this.fixed.is_none() {
                            // Start somewhere sensible: the default schema / current database.
                            let own = this.state.read(cx).database();
                            let preferred = if mysql { own.clone() } else { "public".to_string() };
                            if this.schema.is_empty() {
                                this.schema = this.schemas.iter().find(|s| s.name == preferred).or(this.schemas.first()).map(|s| s.name.clone()).unwrap_or_default();
                            }
                            if this.database.is_empty() {
                                this.database = this.databases.iter().find(|d| **d == own).or(this.databases.first()).cloned().unwrap_or_default();
                            }
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => this.error = Some(e.to_string()),
                }
                this.loading = false;
                this.after = this.before(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The object picked so far, or `None` while incomplete.
    fn object(&self, cx: &App) -> Option<GrantObject> {
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let need = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Some(match self.kind {
            GrantObjectKind::Server => GrantObject::Server,
            GrantObjectKind::Database => {
                let name = if self.db_kind(cx) == DatabaseKind::Postgres { self.state.read(cx).database() } else { self.database.clone() };
                GrantObject::Database { name: need(&name)? }
            }
            GrantObjectKind::Schema => GrantObject::Schema { name: need(&self.schema)? },
            GrantObjectKind::AllTables => GrantObject::AllTables { schema: need(&self.schema)? },
            GrantObjectKind::AllSequences => GrantObject::AllSequences { schema: need(&self.schema)? },
            GrantObjectKind::Table => GrantObject::Table { schema: need(&self.schema)?, name: need(&self.table)? },
            GrantObjectKind::Sequence => return None,
        })
    }

    /// What the role holds on the object now. For "all tables" that's what every table there has.
    fn before(&self, cx: &App) -> PrivilegeSet {
        let Some(object) = self.object(cx) else { return PrivilegeSet::default() };
        let state = self.state.read(cx);
        if let GrantObject::AllTables { schema } = &object {
            let tables = self.schemas.iter().find(|s| s.name == *schema).map(|s| s.tables.clone()).unwrap_or_default();
            let sets: Vec<PrivilegeSet> =
                tables.iter().map(|t| state.privileges_on(&GrantObject::Table { schema: schema.clone(), name: t.name.clone() })).collect();
            let Some(first) = sets.first() else { return PrivilegeSet::default() };
            let common: Vec<String> = first.privileges.iter().filter(|p| sets.iter().all(|s| s.privileges.contains(p))).cloned().collect();
            let grantable = !common.is_empty() && sets.iter().all(|s| s.grantable);
            return PrivilegeSet { privileges: common, grantable };
        }
        state.privileges_on(&object)
    }

    /// Checkboxes: the object's privileges, plus any odd ones it already holds.
    fn available(&self, cx: &App) -> Vec<String> {
        let mut list: Vec<String> = access::privileges(self.db_kind(cx), self.kind).iter().map(|s| s.to_string()).collect();
        for p in self.before(cx).privileges {
            if !list.contains(&p) {
                list.push(p);
            }
        }
        list
    }

    fn change(&self, cx: &App) -> Option<AccessChange> {
        let object = self.object(cx)?;
        Some(AccessChange::SetPrivileges { role: self.role.clone(), object, before: self.before(cx), after: self.after.clone() })
    }

    pub fn can_apply(&self, cx: &App) -> bool {
        !self.saving && self.change(cx).is_some_and(|c| self.state.read(cx).preview(&[c]).is_ok_and(|s| !s.is_empty()))
    }

    /// After picking another object: start from what the role has there.
    fn object_changed(&mut self, cx: &mut Context<Self>) {
        self.after = self.before(cx);
        self.error = None;
        cx.notify();
    }

    fn toggle(&mut self, privilege: String, on: bool, cx: &mut Context<Self>) {
        let available = self.available(cx);
        if on {
            // Keep the server's order.
            self.after.privileges = available.into_iter().filter(|p| self.after.privileges.contains(p) || *p == privilege).collect();
        } else {
            self.after.privileges.retain(|p| *p != privilege);
        }
        self.error = None;
        cx.notify();
    }

    pub fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(change) = self.change(cx).filter(|_| self.can_apply(cx)) else { return };
        self.saving = true;
        let task = self.state.update(cx, |s, cx| s.apply(vec![change], cx));
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => window.close_dialog(cx),
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn name_picker(&self, id: &'static str, current: &str, names: Vec<String>, set: fn(&mut Self, String), cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let current = current.to_string();
        Button::new(id)
            .outline()
            .small()
            .label(if current.is_empty() { "Choose…".to_string() } else { current.clone() })
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for name in &names {
                    let (this, name) = (this.clone(), name.clone());
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(name == current).on_click(move |_, _, cx| {
                        let name = name.clone();
                        this.update(cx, |e, cx| {
                            set(e, name);
                            e.object_changed(cx);
                        })
                        .ok();
                    }));
                }
                menu
            })
    }
}

impl Render for PrivilegeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (features, kind) = {
            let s = self.state.read(cx);
            (s.features.clone(), s.kind())
        };
        if self.loading {
            return h_flex().h(px(200.)).justify_center().child(Spinner::new()).into_any_element();
        }
        let muted = cx.theme().muted_foreground;
        let mut object = group("On", cx);
        if self.fixed.is_some() {
            let o = self.object(cx).map(|o| format!("{} · {}", o.kind().title(), o.title())).unwrap_or_default();
            object = object.child(div().text_sm().child(o));
        } else {
            let this = cx.entity().downgrade();
            let kinds: Vec<GrantObjectKind> = features.object_kinds.iter().copied().filter(|k| *k != GrantObjectKind::Sequence).collect();
            let current = self.kind;
            object = object.child(form_row(
                "Type",
                Button::new("object-kind").outline().small().label(current.title()).dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
                    for k in kinds.iter().copied() {
                        let this = this.clone();
                        menu = menu.item(PopupMenuItem::new(k.title()).checked(k == current).on_click(move |_, _, cx| {
                            this.update(cx, |e, cx| {
                                e.kind = k;
                                e.object_changed(cx);
                            })
                            .ok();
                        }));
                    }
                    menu
                }),
                cx,
            ));
            let schema_names: Vec<String> = self.schemas.iter().map(|s| s.name.clone()).collect();
            match self.kind {
                GrantObjectKind::Server => {
                    object = object.child(form_row("Scope", div().text_sm().text_color(muted).child("Every database (*.*)"), cx));
                }
                GrantObjectKind::Database if kind == DatabaseKind::Postgres => {
                    object = object.child(form_row("Database", div().text_sm().child(self.state.read(cx).database()), cx));
                }
                GrantObjectKind::Database => {
                    let picker = self.name_picker("pick-database", &self.database.clone(), self.databases.clone(), |e, v| e.database = v, cx);
                    object = object.child(form_row("Database", picker, cx));
                }
                _ => {
                    let label = if kind == DatabaseKind::Mysql { "Database" } else { "Schema" };
                    let picker = self.name_picker("pick-schema", &self.schema.clone(), schema_names, |e, v| {
                        e.schema = v;
                        e.table.clear();
                    }, cx);
                    object = object.child(form_row(label, picker, cx));
                    if self.kind == GrantObjectKind::Table {
                        let tables: Vec<String> =
                            self.schemas.iter().find(|s| s.name == self.schema).map(|s| s.tables.iter().map(|t| t.name.clone()).collect()).unwrap_or_default();
                        let picker = self.name_picker("pick-table", &self.table.clone(), tables, |e, v| e.table = v, cx);
                        object = object.child(form_row("Table", picker, cx));
                    }
                }
            }
            let hint = match self.kind {
                GrantObjectKind::AllTables => Some("Applies to the tables and views in the schema now; tables created later aren’t included."),
                GrantObjectKind::AllSequences => Some(
                    "Applies to the sequences in the schema now. Inserting into tables with serial or identity columns needs USAGE on their sequences.",
                ),
                _ if features.grants_per_database => Some("Postgres privileges are per database: pick another database in the tables column to manage its privileges."),
                _ => None,
            };
            if let Some(hint) = hint {
                object = object.child(div().text_xs().text_color(muted).child(hint));
            }
        }

        let ready = self.object(cx).is_some();
        let mut privileges = group("Privileges", cx).child(
            h_flex()
                .gap_1()
                .child(Button::new("all").ghost().xsmall().label("All").disabled(!ready).on_click(cx.listener(|this, _, _, cx| {
                    this.after.privileges = this.available(cx);
                    cx.notify();
                })))
                .child(Button::new("none").ghost().xsmall().label("None").disabled(!ready).on_click(cx.listener(|this, _, _, cx| {
                    this.after.privileges.clear();
                    cx.notify();
                }))),
        );
        let mut grid = h_flex().flex_wrap().gap_x_4().gap_y_1();
        for (i, privilege) in self.available(cx).into_iter().enumerate() {
            let checked = self.after.privileges.contains(&privilege);
            let p = privilege.clone();
            grid = grid.child(
                div().w(px(170.)).child(
                    Checkbox::new(("privilege", i))
                        .checked(checked)
                        .disabled(!ready)
                        .label(privilege)
                        .on_click(cx.listener(move |this, on: &bool, _, cx| this.toggle(p.clone(), *on, cx))),
                ),
            );
        }
        privileges = privileges.child(grid).child(
            Checkbox::new("grantable")
                .checked(self.after.grantable)
                .disabled(!ready)
                .label("With grant option (the role may grant these to others)")
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    this.after.grantable = *on;
                    cx.notify();
                })),
        );

        let mut sql = group("SQL", cx);
        sql = match self.change(cx).map(|c| self.state.read(cx).preview(&[c])) {
            None => sql.child(div().text_sm().text_color(muted).child("Choose what to grant privileges on.")),
            Some(Ok(statements)) if statements.is_empty() => sql.child(div().text_sm().text_color(muted).child("Nothing changed.")),
            Some(Ok(statements)) => sql.child(sql_box(statements.iter().map(|s| format!("{};", s.display)).collect::<Vec<_>>().join("\n"), cx)),
            Some(Err(e)) => sql.child(div().text_sm().text_color(muted).child(e)),
        };

        let for_role = format!(
            "For {}{}",
            self.role.title(),
            if features.grants_per_database { format!(" in {}", self.state.read(cx).database()) } else { String::new() }
        );
        v_flex()
            .gap_4()
            .child(div().text_sm().text_color(muted).child(for_role))
            .child(object)
            .child(privileges)
            .child(sql)
            .children(self.error.clone().map(|e| div().text_sm().text_color(cx.theme().red).child(e)))
            .into_any_element()
    }
}

pub fn open_privilege_editor(state: Entity<UsersState>, role: RoleRef, object: Option<GrantObject>, window: &mut Window, cx: &mut App) {
    let title = if object.is_some() { "Edit Privileges" } else { "Grant Privileges" };
    let editor = cx.new(|cx| PrivilegeEditor::new(state, role, object, cx));
    window.open_dialog(cx, move |modal, _, cx| {
        let (saving, ready) = {
            let e = editor.read(cx);
            (e.saving, e.can_apply(cx))
        };
        let footer = h_flex()
            .w_full()
            .gap_2()
            .when(saving, |f| f.child(Spinner::new().small()))
            .child(div().flex_1())
            .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
            .child(Button::new("apply").primary().label("Apply").disabled(!ready).on_click({
                let editor = editor.clone();
                move |_, window, cx| editor.update(cx, |e, cx| e.apply(window, cx))
            }));
        modal.title(title).w(px(600.)).child(editor.clone()).footer(footer)
    });
}

/// Copies a role's name (for the list's menu).
pub fn copy_name(role: &Role, cx: &mut App) {
    copy(role.name.clone(), cx);
}

/// "3 roles", "1 user".
pub fn count_label(kind: DatabaseKind, n: usize) -> String {
    plural(n, &noun(kind).to_lowercase())
}
