//! The hub daemon: one request per connection, JSON lines (stew-proto). It never touches a
//! stew's files; it reaches remotes with `git` and sites through their drivers.
//!
//! Under systemd it takes its socket from socket activation (`LISTEN_FDS`) and, with
//! `--idle-exit`, exits when no request has come for that long and nothing is in progress.

use std::collections::BTreeSet;
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use std::{fs, thread};

use stew_proto::{Call, Reply, Request, VERSION, recv, send};

use crate::catalog::Catalog;
use crate::config::Config;
use crate::error::{Error, Result, fail};
use crate::mirror::Mirror;
use crate::paths::Paths;
use crate::refs;
use crate::state::StateFile;

pub struct Hub {
    pub paths: Paths,
    /// The socket clients reach this hub on (the host driver's `STEW_HUB`).
    pub socket: PathBuf,
    /// The dir holding the in-box client as `stew` (host stews get it first on `PATH`).
    pub client: Option<PathBuf>,
    /// Serializes git in the mirror (concurrent fetches fight over its lock files).
    mirror: Mutex<()>,
    pub state: Mutex<StateFile>,
    /// Stews a command is working on.
    busy: Mutex<BTreeSet<String>>,
    /// Requests being handled, and when the last one ended (for `--idle-exit`).
    active: AtomicUsize,
    last: Mutex<Instant>,
}

impl Hub {
    /// Loads the state file and marks creations a previous daemon left running as failed.
    pub fn open(paths: Paths, socket: PathBuf) -> Result<Self> {
        let mut state = StateFile::load(&paths.state_file())?;
        let failed = state.recover();
        if !failed.is_empty() {
            eprintln!(
                "stew daemon: marked failed (interrupted): {}",
                failed.join(", ")
            );
            state.save()?;
        }
        Ok(Hub {
            paths,
            socket,
            client: client_dir(),
            mirror: Mutex::new(()),
            state: Mutex::new(state),
            busy: Mutex::new(BTreeSet::new()),
            active: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        })
    }

    pub fn mirror_lock(&self) -> MutexGuard<'_, ()> {
        self.mirror.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn busy_ids(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.busy.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Idle: no request being handled, no creation in progress, none for `idle`.
    fn idle_for(&self, idle: Duration) -> bool {
        self.active.load(Ordering::SeqCst) == 0
            && !self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .in_progress()
            && self
                .last
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .elapsed()
                >= idle
    }

    pub fn handle(&self, req: Request) -> Reply {
        let res = self
            .caller(&req)
            .and_then(|caller| self.dispatch(req.call, caller.as_deref()));
        match res {
            Ok(r) => r,
            Err(e) => Reply::Error {
                kind: e.kind(),
                message: e.to_string(),
            },
        }
    }

    /// The stew a request comes from: none (the host CLI), or the stew it names, which must
    /// be on a site and carry that stew's token, and may only make a stew client's calls.
    fn caller(&self, req: &Request) -> Result<Option<String>> {
        let Some(id) = &req.stew else {
            if req.token.is_some() {
                return Err(Error::Refused("a token without a stew".into()));
            }
            return Ok(None);
        };
        crate::id::check(id).map_err(|e| Error::Refused(e.to_string()))?;
        let token = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .get(id)
            .map(|p| p.token.clone())
            .ok_or_else(|| Error::Refused(format!("stew {id} is on no site")))?;
        if !req.token.as_deref().is_some_and(|t| same(t, &token)) {
            return Err(Error::Refused(format!(
                "this is not stew {id}'s token (STEW_TOKEN)"
            )));
        }
        if !req.call.from_stew() {
            return Err(Error::Refused(
                "that is not a call a stew's client makes".into(),
            ));
        }
        Ok(Some(id.clone()))
    }

    fn dispatch(&self, call: Call, caller: Option<&str>) -> Result<Reply> {
        // config and catalog are read per request, so edits apply without a restart
        let config = Config::load(&self.paths)?;
        match call {
            Call::Hello => Ok(Reply::Hello { version: VERSION }),
            Call::Repos => {
                let cat = Catalog::load(&self.paths, &config)?;
                Ok(Reply::Repos { repos: cat.repos() })
            }
            Call::Resolve { repos, base, here } => {
                let cat = Catalog::load(&self.paths, &config)?;
                let entries = repos
                    .iter()
                    .map(|r| cat.find(r))
                    .collect::<Result<Vec<_>>>()?;
                let (repos, warnings) =
                    refs::resolve_new(&entries, base.as_deref(), here.as_ref())?;
                Ok(Reply::Resolved { repos, warnings })
            }
            Call::New {
                repos,
                title,
                site,
                branch,
                base,
                here,
                from,
                brief,
                start,
                agent,
                model,
            } => self.new_stew(crate::ops::NewArgs {
                repos,
                title,
                site,
                branch,
                base,
                here,
                // from a stew, always the caller
                from: caller.map(String::from).or(from),
                brief,
                start,
                agent,
                model,
            }),
            Call::Info => match caller {
                Some(id) => self.info(id),
                None => Err(fail!("`info` is for a stew's client (no stew named)")),
            },
            Call::Agent {
                id,
                agent,
                model,
                task,
            } => self.agent(&id, agent.as_deref(), model.as_deref(), &task, caller),
            Call::Attach { agent } => self.attach(&agent),
            Call::LsFrom { id } => {
                let id = match (id.as_str(), caller) {
                    (".", Some(c)) => c.to_string(),
                    (".", None) => return Err(fail!("`--from .` needs a stew (run it in one)")),
                    (i, _) => i.to_string(),
                };
                self.ls_from(&id)
            }
            Call::Skills => {
                let mirror = match config.artifacts {
                    Some(_) => Some(self.mirror(&config)?),
                    None => None,
                };
                let _lock = self.mirror_lock();
                let warnings = crate::skills::assemble(&self.paths.skills(), mirror.as_ref())?;
                Ok(Reply::Done { warnings })
            }
            Call::Open { id, site } => self.open_stew(&id, site.as_deref()),
            Call::Remove { id, force } => self.remove(&id, force, false),
            Call::Close { id, force } => self.remove(&id, force, true),
            Call::Start { id } => self.start(id.as_deref()),
            Call::Stop { id } => self.stop(&id),
            Call::Title { id, title } => self.title(&id, &title),
            Call::Ls => self.ls(),
            Call::Where {
                id,
                argv,
                no_start,
                tty,
            } => self.launch(&id, &argv, no_start, tty),
            Call::LsAll { search, closed } => {
                let mirror = self.mirror(&config)?;
                let _lock = self.mirror_lock();
                mirror.ensure()?;
                let mut warnings = Vec::new();
                // an unreachable remote is normal: list what the mirror has
                if let Err(e) = mirror.fetch() {
                    warnings.push(format!("{e} (showing the last fetched state)"));
                }
                let (stews, more) = mirror.tracked(search.as_deref(), closed)?;
                warnings.extend(more);
                Ok(Reply::Tracked { stews, warnings })
            }
        }
    }

    pub fn mirror(&self, config: &Config) -> Result<Mirror> {
        let remote = crate::url::normalize(config.artifacts(&self.paths)?)?;
        Ok(Mirror::new(self.paths.mirror(), remote))
    }
}

/// Compares tokens in time independent of where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The in-box client's dir: `STEW_CLIENT_DIR`, else `<this exe's dir>/../libexec/stew`
/// (the package's layout); only when it holds a `stew`.
fn client_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("STEW_CLIENT_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::current_exe()
            .ok()?
            .parent()?
            .join("../libexec/stew"),
    };
    let dir = dir.canonicalize().ok()?;
    dir.join("stew").is_file().then_some(dir)
}

/// The socket systemd passed (socket activation: `LISTEN_PID` is us, `LISTEN_FDS` ≥ 1,
/// the first at fd 3), if any.
pub fn activated() -> Option<UnixListener> {
    use std::os::fd::FromRawFd;
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds < 1 {
        return None;
    }
    // SAFETY: systemd hands fd 3 to this process as a listening socket; nothing else in
    // the process owns it.
    Some(unsafe { UnixListener::from_raw_fd(3) })
}

/// Binds `socket` (replacing a stale socket file, refusing a live one), owner-only.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir)?;
    }
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(fail!("a daemon already listens on {}", socket.display()));
        }
        fs::remove_file(socket)?;
    }
    let listener =
        UnixListener::bind(socket).map_err(|e| fail!("cannot bind {}: {e}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves until killed (or idle for `idle_exit`), a thread per connection.
pub fn serve(hub: Arc<Hub>, listener: UnixListener, idle_exit: Option<Duration>) -> Result<()> {
    eprintln!("stew daemon: listening on {}", hub.socket.display());
    if let Some(idle) = idle_exit {
        let hub = hub.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1).min(idle));
                if hub.idle_for(idle) {
                    eprintln!("stew daemon: idle, exiting");
                    std::process::exit(0);
                }
            }
        });
    }
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("stew daemon: accept: {e}");
                continue;
            }
        };
        let hub = hub.clone();
        hub.active.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            if let Err(e) = connection(&hub, conn) {
                eprintln!("stew daemon: {e}");
            }
            *hub.last.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
            hub.active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

fn connection(hub: &Hub, conn: UnixStream) -> std::io::Result<()> {
    // one line, of bounded size (a longer one reads as cut off)
    let reply = match recv::<Request>(&mut BufReader::new((&conn).take(stew_proto::MAX_REQUEST))) {
        Ok(Some(req)) => hub.handle(req),
        Ok(None) => return Ok(()),
        Err(e) => Reply::Error {
            kind: stew_proto::ErrorKind::Fail,
            message: format!("bad request: {e}"),
        },
    };
    send(&mut &conn, &reply)
}
