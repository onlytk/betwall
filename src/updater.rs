use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

const OWNER: &str = "onlytk";
const REPO: &str = "betwall";
const BIN_NAME: &str = "betwall";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const INITIAL_DELAY: Duration = Duration::from_secs(30);

#[derive(Default, Clone)]
pub struct UpdateStatus {
    pub latest_version: Option<String>,
    pub last_error: Option<String>,
    pub applying: bool,
}

pub type SharedStatus = Arc<RwLock<UpdateStatus>>;

pub fn shared() -> SharedStatus {
    Arc::new(RwLock::new(UpdateStatus::default()))
}

pub fn spawn_checker(status: SharedStatus, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        sleep_interruptible(INITIAL_DELAY, &stop);
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match fetch_latest() {
                Ok(latest) => {
                    let mut s = status.write().unwrap();
                    s.latest_version = latest;
                    s.last_error = None;
                }
                Err(e) => {
                    status.write().unwrap().last_error = Some(e);
                }
            }
            sleep_interruptible(CHECK_INTERVAL, &stop);
        }
    });
}

fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_secs(5);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step);
        slept += step;
    }
}

fn fetch_latest() -> Result<Option<String>, String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()
        .map_err(|e| e.to_string())?
        .fetch()
        .map_err(|e| e.to_string())?;
    let Some(latest) = releases.first() else {
        return Ok(None);
    };
    let latest_version = latest.version.trim_start_matches('v').to_string();
    let current = env!("CARGO_PKG_VERSION");
    if self_update::version::bump_is_greater(current, &latest_version).unwrap_or(false) {
        Ok(Some(latest_version))
    } else {
        Ok(None)
    }
}

pub fn check_now(status: SharedStatus) -> Result<bool, String> {
    let latest = fetch_latest()?;
    let has_update = latest.is_some();
    let mut s = status.write().unwrap();
    s.latest_version = latest;
    s.last_error = None;
    Ok(has_update)
}

pub fn apply(status: SharedStatus, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        status.write().unwrap().applying = true;
        let result = do_apply();
        {
            let mut s = status.write().unwrap();
            s.applying = false;
            match &result {
                Ok(()) => {
                    s.last_error = None;
                    s.latest_version = None;
                }
                Err(e) => {
                    s.last_error = Some(e.clone());
                }
            }
        }
        if result.is_ok() {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).spawn();
            }
            stop.store(true, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(500));
            std::process::exit(0);
        }
    });
}

fn do_apply() -> Result<(), String> {
    self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name(BIN_NAME)
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .current_version(env!("CARGO_PKG_VERSION"))
        .build()
        .map_err(|e| e.to_string())?
        .update()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
