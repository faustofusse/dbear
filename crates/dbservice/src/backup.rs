//! Dump and restore: the options the apps build per kind of database, file names, and runs that
//! take a target and fill both secrets.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dbcore::dump::{self, CancelToken, Compression, DataStyle, DumpContent, DumpOptions, DumpProgress, DumpScope, DumpSummary, ProgressFn};
use dbcore::restore::{self, RestoreOptions, RestoreProgress, RestoreSummary};
use dbcore::{ConnectionConfig, DatabaseKind, Result, Schema, TableInfo};
use serde::{Deserialize, Serialize};

use crate::{Service, Target};

/// What a dump dialog asks, before the per-kind rules ([`dump_options`]). Defaults are the dialog's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DumpChoices {
    pub content: DumpContent,
    pub compression: Compression,
    pub data_style: DataStyle,
    pub drop_objects: bool,
    pub create_database: bool,
}

impl Default for DumpChoices {
    fn default() -> Self {
        Self {
            content: DumpContent::SchemaAndData,
            compression: Compression::None,
            data_style: DataStyle::Copy,
            drop_objects: false,
            create_database: false,
        }
    }
}

/// The options for dumping `scope` of a `kind` database (see [`normalize_dump_options`]).
pub fn dump_options(kind: DatabaseKind, choices: DumpChoices, scope: DumpScope) -> DumpOptions {
    normalize_dump_options(
        kind,
        DumpOptions {
            content: choices.content,
            scope,
            compression: choices.compression,
            data_style: choices.data_style,
            drop_objects: choices.drop_objects,
            create_database: choices.create_database,
        },
    )
}

/// Only what applies: COPY data only for Postgres (INSERTs elsewhere), no `DROP`s nor
/// `CREATE DATABASE` in a data-only dump, `CREATE DATABASE` only for MySQL.
pub fn normalize_dump_options(kind: DatabaseKind, mut options: DumpOptions) -> DumpOptions {
    if kind != DatabaseKind::Postgres {
        options.data_style = DataStyle::Insert;
    }
    let schema = options.content != DumpContent::DataOnly;
    options.drop_objects &= schema;
    options.create_database &= schema && kind == DatabaseKind::Mysql;
    options
}

/// What to dump, from the tables checked (`(schema, table)`) among `schemas`: the whole database
/// when `everything` or all of it is checked (and no schema is empty), whole schemas (with their
/// functions, types…) when only whole schemas are, else just the tables.
pub fn dump_scope(schemas: &[Schema], selected: &BTreeSet<(String, String)>, everything: bool) -> DumpScope {
    if everything {
        return DumpScope::Database;
    }
    let picked = |t: &TableInfo| selected.contains(&(t.schema.clone(), t.name.clone()));
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

/// A restore dialog's starting options: one transaction (all or nothing) except on MySQL, which
/// commits schema changes as it goes; stop at the first error.
pub fn default_restore_options(kind: DatabaseKind) -> RestoreOptions {
    RestoreOptions { single_transaction: kind != DatabaseKind::Mysql, stop_on_error: true }
}

/// The options for what a restore dialog asks (see [`normalize_restore_options`]).
pub fn restore_options(kind: DatabaseKind, single_transaction: bool, stop_on_error: bool) -> RestoreOptions {
    normalize_restore_options(kind, RestoreOptions { single_transaction, stop_on_error })
}

/// No transaction on MySQL; a transaction always stops at the first error (an error aborts it).
pub fn normalize_restore_options(kind: DatabaseKind, options: RestoreOptions) -> RestoreOptions {
    let transaction = kind != DatabaseKind::Mysql && options.single_transaction;
    RestoreOptions { single_transaction: transaction, stop_on_error: transaction || options.stop_on_error }
}

/// Today's date (UTC) as `2025-06-01`, for default file names.
pub fn today() -> String {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400) as i64;
    civil_date(days)
}

/// Days since 1970-01-01 as `2025-06-01` (Howard Hinnant's civil-from-days).
pub fn civil_date(days: i64) -> String {
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

/// `app_dev-2025-06-01.sql` (`.sql.gz` gzipped) for a dump of `config`'s database today.
pub fn default_dump_file_name(config: &ConnectionConfig, compression: Compression) -> String {
    dump::default_file_name(&dump::target_name(config), &today(), compression)
}

impl Service {
    /// [`default_dump_file_name`] for the target.
    pub fn dump_file_name(&self, target: &Target, compression: Compression) -> Result<String> {
        Ok(default_dump_file_name(&self.target_config(target)?, compression))
    }

    /// Dumps the target's database into `path` with its own session (and tunnel), the password and
    /// SSH secret read from the keychain. Options are normalized for the kind first. `progress` is
    /// called from the core's runtime (about 10 times a second).
    pub async fn dump(
        &self,
        target: &Target,
        path: PathBuf,
        options: DumpOptions,
        progress: ProgressFn<DumpProgress>,
        cancel: CancelToken,
    ) -> Result<DumpSummary> {
        let config = self.config_with_secrets(target).await?;
        let options = normalize_dump_options(config.kind, options);
        dump::dump(config, path, options, progress, cancel).await
    }

    /// Runs the script at `path` against the target's database (like [`dump`](Self::dump)). When
    /// it ends, also on failure or cancel, the target's schemas are dropped: tables may have changed.
    pub async fn restore(
        &self,
        target: &Target,
        path: PathBuf,
        options: RestoreOptions,
        progress: ProgressFn<RestoreProgress>,
        cancel: CancelToken,
    ) -> Result<RestoreSummary> {
        let config = self.config_with_secrets(target).await?;
        let options = normalize_restore_options(config.kind, options);
        let result = restore::restore(config, path, options, progress, cancel).await;
        self.schema_changed(target);
        result
    }
}
