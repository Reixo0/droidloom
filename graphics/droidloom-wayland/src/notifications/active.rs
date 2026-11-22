//! The apps Android's task manager lists (SystemUI's "apps are active"), as
//! `org.droidloom.ActiveApps` for the desktop's quick settings.
use super::media::Commands;
use serde::Deserialize;
use std::sync::{Arc, Mutex, PoisonError};
use zbus::{Connection, interface};

const NAME: &str = "org.droidloom.ActiveApps";
const PATH: &str = "/org/droidloom/ActiveApps";
const MAX: usize = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct App {
    pub(super) package: String,
    pub(super) label: String,
    /// The launcher activity, `package/class`, or empty.
    pub(super) component: String,
    /// Milliseconds since the epoch.
    pub(super) since: i64,
    pub(super) stoppable: bool,
}

/// What Android may send: every text bounded, one entry for each package.
pub(super) fn valid(apps: &[App]) -> bool {
    apps.len() <= MAX
        && apps.iter().enumerate().all(|(i, app)| {
            let text = |s: &String, max| s.len() <= max && !s.contains('\0');
            !app.package.is_empty()
                && text(&app.package, 255)
                && text(&app.label, 1024)
                && app.since >= 0
                && (app.component.is_empty()
                    || (app.component.len() <= 511
                        && app
                            .component
                            .strip_prefix(&app.package)
                            .is_some_and(|class| class.len() > 1 && class.starts_with('/'))))
                && apps[..i].iter().all(|other| other.package != app.package)
        })
}

type Shared = Arc<Mutex<Vec<App>>>;

struct Object {
    apps: Shared,
    commands: Commands,
}
#[interface(name = "org.droidloom.ActiveApps")]
impl Object {
    /// Each app: its package, its label, its desktop entry (or empty), since
    /// when it is active (milliseconds since the epoch) and whether the user
    /// may stop it.
    #[zbus(property)]
    fn apps(&self) -> Vec<(String, String, String, i64, bool)> {
        lock(&self.apps)
            .iter()
            .map(|app| {
                let entry = if app.component.is_empty() {
                    String::new()
                } else {
                    droidloom_contracts::applications::application_id(0, &app.component)
                };
                (
                    app.package.clone(),
                    app.label.clone(),
                    entry,
                    app.since,
                    app.stoppable,
                )
            })
            .collect()
    }
    /// Stops a listed app, as the task manager's Stop button.
    fn stop(&self, package: &str) {
        if lock(&self.apps)
            .iter()
            .any(|app| app.stoppable && app.package == package)
        {
            let _ = self
                .commands
                .try_send(serde_json::json!({"type":"stop","package":package}));
        }
    }
}

fn lock(apps: &Shared) -> std::sync::MutexGuard<'_, Vec<App>> {
    apps.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The list on the session bus.
pub(super) struct Service {
    apps: Shared,
    conn: Connection,
}
impl Service {
    pub(super) async fn start(conn: &Connection, commands: Commands) -> Self {
        let apps = Shared::default();
        let object = Object {
            apps: apps.clone(),
            commands,
        };
        let served = async {
            conn.object_server().at(PATH, object).await?;
            conn.request_name(NAME).await
        };
        if let Err(error) = served.await {
            eprintln!("Droidloom active apps: not on the session bus ({error})");
        }
        Self {
            apps,
            conn: conn.clone(),
        }
    }

    pub(super) async fn set(&self, apps: Vec<App>) {
        {
            let mut current = lock(&self.apps);
            if *current == apps {
                return;
            }
            *current = apps;
        }
        if let Ok(object) = self.conn.object_server().interface::<_, Object>(PATH).await {
            let emitter = object.signal_emitter();
            let _ = object.get().await.apps_changed(emitter).await;
        }
    }
}
