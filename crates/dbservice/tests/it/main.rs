//! dbservice's tests, in one binary (each test binary links all of dbcore). No network, no real
//! keychain: temp stores, SQLite files, `MemorySecretStore` and dbcore's mock driver.

mod backup;
mod crud;
mod lookup;
mod schemas;
mod scripts;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use dbcore::secrets::{MemorySecretStore, SecretStore};
use dbcore::{ConnectionConfig, DatabaseKind};
use dbservice::{Options, SaveRequest, Secrets, Service};

/// A minimal executor: proves the service needs no tokio runtime from its caller.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub service: Service,
    pub secrets: Arc<MemorySecretStore>,
}

#[allow(clippy::new_without_default)]
impl Fixture {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(tweak: impl FnOnce(&mut Options)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let mut options = Options {
            store_path: Some(dir.path().join("dbear.db")),
            state_path: None,
            secrets: Secrets::Store(secrets.clone() as Arc<dyn SecretStore>),
            oauth_sync: false,
            samples: false,
        };
        tweak(&mut options);
        let service = Service::open(options).unwrap();
        Self { dir, service, secrets }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Saves a SQLite connection to `file` (in the temp dir, created empty when its folder exists) called `name`.
    pub fn sqlite(&self, name: &str, file: &str) -> ConnectionConfig {
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(self.path(file));
        let config = sqlite_config(name, &self.path(file));
        self.service.save(SaveRequest::new(config)).unwrap().config
    }

    /// Saves a Postgres connection (never connected to).
    pub fn postgres(&self, name: &str, database: &str, show_all: bool) -> ConnectionConfig {
        let mut config = ConnectionConfig::new_empty(DatabaseKind::Postgres);
        config.name = name.into();
        config.host = "db.invalid".into();
        config.database = database.into();
        config.user = Some("app".into());
        config.show_all_databases = show_all;
        self.service.save(SaveRequest::new(config)).unwrap().config
    }
}

pub fn sqlite_config(name: &str, path: &Path) -> ConnectionConfig {
    let mut config = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
    config.name = name.into();
    config.database = path.display().to_string();
    config
}

#[test]
fn service_and_its_futures_are_send() {
    fn send_sync<T: Send + Sync + Clone + 'static>() {}
    fn send<T: Send>(_: &T) {}
    send_sync::<Service>();
    let fx = Fixture::new();
    let (service, target) = (fx.service.clone(), dbservice::Target::new("x", "y"));
    let connection = dbcore::Connection::new(sqlite_config("x", &fx.path("x.db")));
    send(&service.load(&target, false));
    send(&service.connection(&target));
    send(&service.catalog(&target));
    send(&service.complete(&target, "", 0));
    send(&service.run_script(&target, &connection, String::new(), Default::default()));
    send(&service.run(&target, String::new(), Default::default()));
    send(&service.test(sqlite_config("x", &fx.path("x.db")), false));
    send(&service.disconnect_all());
    send(&service.lost_connections());
    send(&service.create_database("x", "y"));
    let progress: dbcore::dump::ProgressFn<dbcore::dump::DumpProgress> = Arc::new(|_| {});
    send(&service.dump(&target, fx.path("d.sql"), Default::default(), progress, dbcore::dump::CancelToken::new()));
}
