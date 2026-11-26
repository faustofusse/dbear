use dbcore::secrets::{self, SecretStore};
use dbcore::{ConnectionConfig, DatabaseKind, Error, SshAuth, SshTunnel};
use dbservice::{RunOptions, SaveRequest, SecretKind, Secrets, Target};

use crate::{block_on, sqlite_config, Fixture};

fn postgres(name: &str) -> ConnectionConfig {
    let mut config = ConnectionConfig::new_empty(DatabaseKind::Postgres);
    config.name = name.into();
    config.host = "db.invalid".into();
    config.database = "app".into();
    config.user = Some("app".into());
    config
}

fn tunnel(auth: SshAuth) -> Option<SshTunnel> {
    Some(SshTunnel { host: "bastion".into(), user: "me".into(), auth, key_path: "~/.ssh/id".into(), ..Default::default() })
}

#[test]
fn saves_the_store_then_the_keychain() {
    let fx = Fixture::new();
    let ssh = |id: &str| fx.secrets.password(&secrets::ssh_account(id)).unwrap();
    let mut config = postgres("pg");
    config.ssh = tunnel(SshAuth::Password);
    config.password = Some("ignored: only the request's password is saved".into());
    let saved = fx.service.save(SaveRequest { config, password: Some("pw".into()), ssh_secret: Some("sshpw".into()) }).unwrap();
    assert!(saved.problems.is_empty());
    let id = saved.config.id.clone();
    assert!(!id.is_empty());
    assert_eq!(saved.config.password, None);
    assert_eq!(fx.service.connections(), vec![saved.config.clone()]);
    assert_eq!(fx.secrets.password(&id).unwrap().as_deref(), Some("pw"));
    assert_eq!(ssh(&id).as_deref(), Some("sshpw"));

    // Saving again without typing anything keeps both.
    let saved = fx.service.save(SaveRequest::new(saved.config)).unwrap();
    assert_eq!(fx.secrets.password(&id).unwrap().as_deref(), Some("pw"));
    assert_eq!(ssh(&id).as_deref(), Some("sshpw"));

    // The agent needs no secret: it's dropped (even if one was typed).
    let mut agent = saved.config.clone();
    agent.ssh = tunnel(SshAuth::Agent);
    fx.service.save(SaveRequest { config: agent, password: None, ssh_secret: Some("typed".into()) }).unwrap();
    assert_eq!(ssh(&id), None);

    // Back to a key, with a passphrase; then no tunnel at all drops it again.
    let mut key = saved.config.clone();
    key.ssh = tunnel(SshAuth::PrivateKey);
    fx.service.save(SaveRequest { config: key, password: None, ssh_secret: Some("phrase".into()) }).unwrap();
    assert_eq!(ssh(&id).as_deref(), Some("phrase"));
    let mut plain = saved.config.clone();
    plain.ssh = None;
    fx.service.save(SaveRequest::new(plain.clone())).unwrap();
    assert_eq!(ssh(&id), None);

    // An empty password deletes the saved one.
    fx.service.save(SaveRequest { config: plain, password: Some(String::new()), ssh_secret: None }).unwrap();
    assert_eq!(fx.secrets.password(&id).unwrap(), None);
    assert_eq!(fx.service.connections().len(), 1);
}

#[test]
fn saving_forgets_the_old_connection() {
    let fx = Fixture::new();
    let config = fx.sqlite("app", "app.db");
    let target = Target::default_for(&config);
    block_on(fx.service.load(&target, false)).unwrap();
    assert!(fx.service.cached(&target).is_some());
    let mut renamed = config.clone();
    renamed.name = "renamed".into();
    let saved = fx.service.save(SaveRequest::new(renamed)).unwrap();
    assert_eq!(saved.config.id, config.id);
    assert!(fx.service.cached(&target).is_none() && fx.service.cached_schemas(&target).is_none());
    assert_eq!(fx.service.find("renamed").unwrap().id, config.id);
}

#[test]
fn invalid_configs_arent_saved() {
    let fx = Fixture::new();
    let mut config = postgres("pg");
    config.host = String::new();
    assert!(matches!(fx.service.save(SaveRequest { config, password: Some("pw".into()), ssh_secret: None }), Err(Error::InvalidConfig(_))));
    assert!(fx.service.connections().is_empty());
}

#[test]
fn keychain_failures_dont_undo_the_save() {
    let fx = Fixture::with(|o| o.secrets = Secrets::Disabled);
    let saved = fx.service.save(SaveRequest { config: postgres("pg"), password: Some("pw".into()), ssh_secret: None }).unwrap();
    assert_eq!(saved.problems.len(), 1);
    assert_eq!(saved.problems[0].secret, SecretKind::Password);
    assert_eq!(saved.problems[0].to_string(), "Saved, but the password wasn’t: Unsupported: no keyring");
    assert_eq!(fx.service.connections().len(), 1);
}

#[test]
fn duplicates_with_both_secrets() {
    let fx = Fixture::new();
    let mut config = postgres("pg");
    config.ssh = tunnel(SshAuth::Password);
    let original = fx.service.save(SaveRequest { config, password: Some("pw".into()), ssh_secret: Some("sshpw".into()) }).unwrap().config;
    let copy = fx.service.duplicate(&original).unwrap();
    assert!(copy.problems.is_empty());
    let copy = copy.config;
    assert_ne!(copy.id, original.id);
    assert_eq!(copy.name, "pg copy");
    assert_eq!(copy.host, original.host);
    assert_eq!(fx.secrets.password(&copy.id).unwrap().as_deref(), Some("pw"));
    assert_eq!(fx.secrets.password(&secrets::ssh_account(&copy.id)).unwrap().as_deref(), Some("sshpw"));
    assert_eq!(fx.service.connections().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["pg", "pg copy"]);
}

#[test]
fn deletes_everything_about_a_connection() {
    let fx = Fixture::new();
    let mut config = sqlite_config("app", &fx.path("app.db"));
    config.password = None;
    let saved = fx.service.save(SaveRequest { config, password: Some("pw".into()), ssh_secret: None }).unwrap().config;
    fx.secrets.set_password(&secrets::ssh_account(&saved.id), "stray").unwrap();
    let target = Target::default_for(&saved);
    block_on(fx.service.run(&target, "select 1".into(), RunOptions::default())).unwrap();
    assert_eq!(fx.service.history(&saved.id, 10).unwrap().len(), 1);
    assert!(fx.service.cached(&target).is_some());

    assert!(fx.service.delete(&saved.id).unwrap());
    assert!(fx.service.connections().is_empty());
    assert_eq!(fx.secrets.password(&saved.id).unwrap(), None);
    assert_eq!(fx.secrets.password(&secrets::ssh_account(&saved.id)).unwrap(), None);
    assert_eq!(fx.service.history(&saved.id, 10).unwrap(), []);
    assert!(fx.service.cached(&target).is_none());
    assert!(fx.service.find(&saved.id).is_err());
    assert!(!fx.service.delete(&saved.id).unwrap());
}

#[test]
fn renames_only_the_name() {
    let fx = Fixture::new();
    let mut config = postgres("Shop");
    config.show_all_databases = true;
    let saved = fx.service.save(SaveRequest { config, password: Some("pw".into()), ssh_secret: None }).unwrap().config;
    let other = fx.sqlite("other", "other.db");
    fx.service.set_last_database(&saved.id, "billing").unwrap();
    let target = Target::default_for(&other);
    block_on(fx.service.run(&target, "select 1".into(), RunOptions { record_history: true, ..RunOptions::default() })).unwrap();
    // Another process edits the port and the user meanwhile: the rename must not put the old ones back.
    let mut theirs = dbcore::ConnectionStore::open(fx.path("dbear.db")).unwrap();
    theirs.upsert(ConnectionConfig { port: Some(6543), user: Some("admin".into()), ..saved.clone() }).unwrap();

    let renamed = fx.service.rename(&saved.id, " Store ").unwrap();
    assert_eq!(renamed.name, "Store");
    assert_eq!((renamed.port, renamed.user.as_deref()), (Some(6543), Some("admin")), "another process's edit is kept");
    assert_eq!(fx.service.config(&saved.id).unwrap(), renamed, "published");
    assert_eq!(fx.service.find("store").unwrap().id, saved.id);
    assert!(fx.service.find("Shop").is_err());
    assert_eq!(fx.secrets.password(&saved.id).unwrap().as_deref(), Some("pw"), "keychain untouched");
    assert_eq!(fx.service.remembered_database(&renamed).as_deref(), Some("billing"));
    assert_eq!(fx.service.rename(&other.id, "Other").unwrap().name, "Other");
    assert_eq!(fx.service.history(&other.id, 10).unwrap().len(), 1, "history is by id");
    assert!(fx.service.cached(&target).is_some(), "sessions stay");

    let unknown = fx.service.rename("ghost", "x").unwrap_err();
    assert!(matches!(unknown, Error::InvalidConfig(_)), "{unknown:?}");
    let url = fx.service.find(&format!("sqlite:{}", fx.path("other.db").display())).unwrap();
    assert!(fx.service.rename(&url.id, "x").unwrap_err().to_string().contains("URL"));
    let names: Vec<String> = fx.service.connections().into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["Store", "Other"]);

    let samples = Fixture::with(|o| o.samples = true);
    assert!(samples.service.rename("staging-pg", "x").unwrap_err().to_string().contains("Sample"));
}

#[test]
fn imports_with_secrets_and_problems() {
    let fx = Fixture::new();
    let mut good = postgres("imported");
    good.password = Some("pw".into());
    good.ssh = tunnel(SshAuth::Password);
    good.ssh.as_mut().unwrap().secret = Some("sshpw".into());
    let mut bad = postgres("broken");
    bad.host = String::new();
    let mut no_password = postgres("plain");
    no_password.password = Some(String::new());
    let import = fx.service.import(vec![good, bad, no_password]).unwrap();
    assert_eq!(import.imported.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["imported", "plain"]);
    assert_eq!(import.problems.len(), 1);
    assert!(import.problems[0].starts_with("broken: "), "{:?}", import.problems);
    let id = &import.imported[0].id;
    assert_eq!(fx.secrets.password(id).unwrap().as_deref(), Some("pw"));
    assert_eq!(fx.secrets.password(&secrets::ssh_account(id)).unwrap().as_deref(), Some("sshpw"));
    assert_eq!(fx.secrets.password(&import.imported[1].id).unwrap(), None);
    assert_eq!(fx.service.connections().len(), 2);

    let without = Fixture::with(|o| o.secrets = Secrets::Disabled);
    let mut one = postgres("one");
    one.password = Some("pw".into());
    let import = without.service.import(vec![one]).unwrap();
    assert_eq!(import.problems, ["one: the password wasn’t saved (no keyring)"]);
}

#[test]
fn scanning_dbeaver_marks_whats_already_there() {
    let fx = Fixture::new();
    fx.sqlite("app", "app.db");
    let sources = fx.path("data-sources.json");
    let json = serde_json::json!({
        "connections": {
            "a": {"provider": "sqlite", "driver": "sqlite_jdbc", "name": "Same", "configuration": {"url": format!("jdbc:sqlite:{}", fx.path("app.db").display()), "configurationType": "URL"}},
            "b": {"provider": "sqlite", "driver": "sqlite_jdbc", "name": "Other", "configuration": {"url": format!("jdbc:sqlite:{}", fx.path("other.db").display()), "configurationType": "URL"}}
        }
    });
    std::fs::write(&sources, json.to_string()).unwrap();
    let scan = fx.service.scan_dbeaver(Some(&sources)).unwrap();
    let flags: Vec<(&str, bool)> = scan.connections.iter().map(|c| (c.config.name.as_str(), c.already_added)).collect();
    assert!(flags.contains(&("Same", true)) && flags.contains(&("Other", false)), "{flags:?} {:?}", scan.skipped);
}

#[test]
fn tests_connections_with_typed_or_saved_secrets() {
    let fx = Fixture::new();
    std::fs::write(fx.path("ok.db"), "").unwrap();
    block_on(fx.service.test(sqlite_config("ok", &fx.path("ok.db")), false)).unwrap();
    assert!(block_on(fx.service.test(sqlite_config("bad", &fx.path("missing/dir/x.db")), false)).is_err());

    // A sample that can't connect, and one that can (mock: no network).
    let fx = Fixture::with(|o| o.samples = true);
    let replica = fx.service.config("prod-replica").unwrap();
    assert!(matches!(block_on(fx.service.test(replica, true)), Err(Error::ConnectionFailed(_))));
    let staging = fx.service.config("staging-pg").unwrap();
    block_on(fx.service.test(staging, true)).unwrap();
}

#[test]
fn urls_include_the_saved_password_when_asked() {
    let fx = Fixture::new();
    let saved = fx.service.save(SaveRequest { config: postgres("pg"), password: Some("s3cret".into()), ssh_secret: None }).unwrap().config;
    let with = fx.service.url(&saved.id, true).unwrap();
    let without = fx.service.url(&saved.id, false).unwrap();
    assert!(with.contains("s3cret"), "{with}");
    assert!(!without.contains("s3cret"), "{without}");
    assert!(fx.service.url("missing", false).is_err());
}

#[test]
fn a_failed_create_database_keeps_the_list() {
    let fx = Fixture::with(|o| o.samples = true);
    let target = Target::new("staging-pg", "app");
    block_on(fx.service.load(&target, false)).unwrap();
    assert!(fx.service.databases("staging-pg").is_some());
    // The mock driver only runs SELECTs: it fails, over the open session, and the list stays.
    let result = block_on(fx.service.create_database("staging-pg", "fresh"));
    assert!(matches!(result, Err(dbcore::Error::Unsupported(_))), "{result:?}");
    assert!(fx.service.databases("staging-pg").is_some());
    assert!(block_on(fx.service.create_database("missing", "x")).is_err());
}
