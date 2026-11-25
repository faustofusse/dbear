use std::sync::Arc;

use dbcore::secrets::{self, SecretStore};
use dbcore::{DatabaseKind, Error, SshAuth, SshTunnel};
use dbservice::{Options, Part, Secrets, Service, Target};

use crate::Fixture;

#[test]
fn opens_the_state_file_beside_the_store() {
    let fx = Fixture::new();
    assert!(fx.service.has_store() && fx.service.has_state() && fx.service.has_secrets());
    assert!(fx.service.problems().is_empty());
    assert_eq!(fx.service.store_path().unwrap(), fx.path("dbear.db"));
    fx.service.state_set("gpui.session", Some("{}")).unwrap();
    assert!(fx.path("state.db").exists());
    assert_eq!(fx.service.state_get("gpui.session").as_deref(), Some("{}"));
    fx.service.state_set("gpui.session", None).unwrap();
    assert_eq!(fx.service.state_get("gpui.session"), None);
}

#[test]
fn a_store_that_wont_open_fails_strict_and_shows_samples_lenient() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), "").unwrap();
    let options = Options {
        store_path: Some(dir.path().join("file/dbear.db")),
        secrets: Secrets::Disabled,
        oauth_sync: false,
        samples: true,
        ..Options::default()
    };
    assert!(matches!(Service::open(options.clone()), Err(Error::Storage(_))));
    let service = Service::open_lenient(options);
    assert_eq!(service.problems().iter().map(|p| p.part).collect::<Vec<_>>(), [Part::Store]);
    assert!(!service.has_store() && !service.has_state() && !service.has_secrets());
    let (listed, samples) = service.listed_connections();
    assert!(samples);
    assert_eq!(listed.len(), dbcore::mock::connections().len());
    assert_eq!(service.find("app_staging").unwrap().id, "staging-pg");
    assert!(service.save(dbservice::SaveRequest::new(listed[5].clone())).is_err());
}

#[test]
fn a_state_file_that_wont_open_is_a_problem_only() {
    let fx = Fixture::with(|o| {
        let dir = o.store_path.as_ref().unwrap().parent().unwrap().to_path_buf();
        std::fs::write(dir.join("blocker"), "").unwrap();
        o.state_path = Some(dir.join("blocker/state.db"));
    });
    assert_eq!(fx.service.problems().iter().map(|p| p.part).collect::<Vec<_>>(), [Part::State]);
    assert!(fx.service.state_set("k", Some("v")).is_err());
    assert_eq!(fx.service.history("x", 10).unwrap(), []);
}

#[test]
fn honours_dbear_connections_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("elsewhere.db");
    // Only this test opens a service without an explicit store path.
    std::env::set_var(dbservice::CONNECTIONS_FILE_ENV, &path);
    let service = Service::open(Options { secrets: Secrets::Disabled, oauth_sync: false, ..Options::default() });
    std::env::remove_var(dbservice::CONNECTIONS_FILE_ENV);
    assert_eq!(service.unwrap().store_path().unwrap(), path);
}

#[test]
fn no_keyring_env_disables_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    // Only this test sets it; the others never ask for the real keychain.
    std::env::set_var(dbservice::NO_KEYRING_ENV, "1");
    let service = Service::open(Options { store_path: Some(dir.path().join("dbear.db")), oauth_sync: false, ..Options::default() });
    std::env::remove_var(dbservice::NO_KEYRING_ENV);
    let service = service.unwrap();
    assert!(!service.has_secrets());
    assert!(service.problems().is_empty(), "{:?}", service.problems());
}

#[test]
fn finds_by_id_name_or_url() {
    let fx = Fixture::new();
    let app = fx.sqlite("App", "app.db");
    let a = fx.sqlite("Twin", "a.db");
    let b = fx.sqlite("twin", "b.db");
    assert_eq!(fx.service.find(&app.id).unwrap(), app);
    assert_eq!(fx.service.find("  aPP ").unwrap(), app);
    let Err(Error::InvalidConfig(message)) = fx.service.find("TWIN") else { panic!() };
    assert!(message.contains(&a.id) && message.contains(&b.id), "{message}");
    assert_eq!(fx.service.find(&b.id).unwrap(), b);
    assert!(matches!(fx.service.find("nope"), Err(Error::InvalidConfig(_))));
    assert!(matches!(fx.service.find(""), Err(Error::InvalidConfig(_))));
    assert!(matches!(fx.service.find("ftp://x"), Err(Error::InvalidConfig(_))));

    let url = "postgres://me:s3cret@db.example.com:6543/shop";
    let found = fx.service.find(url).unwrap();
    assert!(found.id.starts_with(dbservice::URL_ID_PREFIX));
    assert_eq!((found.kind, found.host.as_str(), found.port, found.password.as_deref()), (DatabaseKind::Postgres, "db.example.com", Some(6543), Some("s3cret")));
    assert_eq!(fx.service.find(url).unwrap().id, found.id, "same URL, same id");
    assert_eq!(fx.service.config(&found.id).unwrap(), found);
    // Not saved, and its password isn't looked up in the keychain.
    assert_eq!(fx.service.connections().len(), 3);
    fx.secrets.set_password(&found.id, "keychain").unwrap();
    assert_eq!(fx.service.fill_secrets(found.clone()).password.as_deref(), Some("s3cret"));
    let target = fx.service.resolve(url, None).unwrap();
    assert_eq!(target, Target::new(found.id.clone(), "shop"));
    assert_eq!(fx.service.url(&found.id, true).unwrap(), found.to_url(true));
}

#[test]
fn resolves_the_remembered_database() {
    let fx = Fixture::new();
    let browsing = fx.postgres("pg", "app", true);
    let single = fx.postgres("single", "app", false);
    let maintenance = fx.postgres("bare", "", true);
    assert_eq!(fx.service.resolve("pg", None).unwrap(), Target::new(browsing.id.clone(), "app"));
    assert_eq!(fx.service.resolve("bare", None).unwrap(), Target::new(maintenance.id.clone(), "postgres"));

    fx.service.set_last_database(&browsing.id, "reports").unwrap();
    fx.service.set_last_database(&single.id, "reports").unwrap();
    assert_eq!(fx.service.resolve("pg", None).unwrap().database, "reports");
    assert_eq!(fx.service.resolve("single", None).unwrap().database, "app", "doesn't browse databases");
    assert_eq!(fx.service.resolve("pg", Some(" shop ")).unwrap().database, "shop");
    assert_eq!(fx.service.resolve("pg", Some("")).unwrap().database, "reports");
    // Going back to its own database forgets the remembered one.
    fx.service.set_last_database(&browsing.id, "app").unwrap();
    assert_eq!(fx.service.remembered_database(&browsing), None);
    assert_eq!(fx.service.resolve("pg", None).unwrap().database, "app");

    let target = Target::new(browsing.id.clone(), "shop");
    assert_eq!(fx.service.target_config(&target).unwrap().database, "shop");
    assert_eq!(fx.service.target_config(&Target::new(maintenance.id.clone(), "postgres")).unwrap().database, "", "its own stays as configured");
    assert_eq!(fx.service.target_label(&target), "pg · shop");
    assert_eq!(fx.service.target_label(&Target::new(single.id.clone(), "app")), "single");
    assert!(fx.service.target_config(&Target::new("missing", "x")).is_err());
}

#[test]
fn fills_both_secrets_except_for_the_agent() {
    let fx = Fixture::new();
    let mut config = fx.postgres("pg", "app", false);
    config.ssh = Some(SshTunnel { host: "bastion".into(), user: "me".into(), auth: SshAuth::Password, ..Default::default() });
    fx.secrets.set_password(&config.id, "db-pass").unwrap();
    fx.secrets.set_password(&secrets::ssh_account(&config.id), "ssh-pass").unwrap();
    let filled = fx.service.fill_secrets(config.clone());
    assert_eq!(filled.password.as_deref(), Some("db-pass"));
    assert_eq!(filled.ssh.as_ref().unwrap().secret.as_deref(), Some("ssh-pass"));

    let mut typed = config.clone();
    typed.password = Some("typed".into());
    assert_eq!(fx.service.fill_secrets(typed).password.as_deref(), Some("typed"));

    config.ssh.as_mut().unwrap().auth = SshAuth::Agent;
    assert_eq!(fx.service.fill_secrets(config.clone()).ssh.unwrap().secret, None);

    // Through the target (on a blocking thread), pointed at its database.
    let mut saved = config.clone();
    saved.ssh.as_mut().unwrap().auth = SshAuth::PrivateKey;
    saved.ssh.as_mut().unwrap().key_path = "~/.ssh/id".into();
    let saved = fx.service.save(dbservice::SaveRequest { config: saved, password: None, ssh_secret: Some("phrase".into()) }).unwrap().config;
    let with = crate::block_on(fx.service.config_with_secrets(&Target::new(saved.id.clone(), "other"))).unwrap();
    assert_eq!((with.database.as_str(), with.password.as_deref()), ("other", Some("db-pass")));
    assert_eq!(with.ssh.unwrap().secret.as_deref(), Some("phrase"));
}

#[test]
fn without_secrets_configs_stay_as_they_are() {
    let fx = Fixture::with(|o| o.secrets = Secrets::Disabled);
    let config = fx.postgres("pg", "app", false);
    assert_eq!(fx.service.fill_secrets(config.clone()).password, None);
    assert!(fx.service.secret_store().is_none());
}

#[test]
fn reload_sees_another_processes_changes() {
    let fx = Fixture::new();
    let other = Service::open(Options {
        store_path: fx.service.store_path().map(Into::into),
        secrets: Secrets::Store(Arc::new(dbcore::secrets::MemorySecretStore::new())),
        oauth_sync: false,
        ..Options::default()
    })
    .unwrap();
    fx.sqlite("Added elsewhere", "x.db");
    assert!(other.connections().is_empty());
    assert!(other.find("added elsewhere").is_err());
    other.reload().unwrap();
    assert_eq!(other.connections().len(), 1);
    assert!(other.find("added elsewhere").is_ok());
}
