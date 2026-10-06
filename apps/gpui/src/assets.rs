//! Icons: GPUI Kit's default component set, plus the few extra Lucide icons dbear uses.

use std::borrow::Cow;

use dbcore::DatabaseKind;
use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::component::Icon;
use gpui_kit::{AssetSource, Result, SharedString};

icon_assets!(ExtraIcons, [TextWrap, Import, ListFilter, Users, Zap, KeyRound, UserPlus]);

/// Engine logos (the macOS app's: Simple Icons, CC0), drawn in the text colour like other icons.
const KIND_ICONS: [(DatabaseKind, &str, &[u8]); 5] = [
    (DatabaseKind::Postgres, "icons/db/postgres.svg", include_bytes!("../assets/db/postgres.svg")),
    (DatabaseKind::Mysql, "icons/db/mysql.svg", include_bytes!("../assets/db/mysql.svg")),
    (DatabaseKind::SqlServer, "icons/db/sqlserver.svg", include_bytes!("../assets/db/sqlserver.svg")),
    (DatabaseKind::Sqlite, "icons/db/sqlite.svg", include_bytes!("../assets/db/sqlite.svg")),
    (DatabaseKind::Libsql, "icons/db/libsql.svg", include_bytes!("../assets/db/libsql.svg")),
];

/// The logo of a connection's engine.
pub fn kind_icon(kind: DatabaseKind) -> Icon {
    let path = KIND_ICONS.iter().find(|(k, _, _)| *k == kind).map_or("icons/db/postgres.svg", |(_, path, _)| *path);
    Icon::empty().path(path)
}

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, _, bytes)) = KIND_ICONS.iter().find(|(_, p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.extend(KIND_ICONS.iter().filter(|(_, p, _)| p.starts_with(path)).map(|(_, p, _)| SharedString::from(*p)));
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
