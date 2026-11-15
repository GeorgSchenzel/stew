//! The hub daemon: one request per connection, JSON lines (stew-proto). It never touches a
//! stew's files; it reaches remotes with `git` and sites through their drivers.
//!
//! It listens on two sockets: the host CLI's (`stew.sock`) and the stews' (`box.sock`, what
//! `STEW_HUB` names: the bento channel's host socket, and the host driver's). On the stews'
//! socket every request must name its stew and carry its token, so the host CLI's calls are
//! never reachable from a stew whatever it sends.
//!
//! Under systemd it takes its sockets from socket activation (`LISTEN_FDS`: the CLI's at
//! fd 3, the stews' at fd 4) and, with `--idle-exit`, exits when no request has come for that
//! long and nothing is in progress.

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
use crate::config::{Config, DriverKind};
use crate::error::{Error, Result, fail};
use crate::mirror::Mirror;
use crate::paths::Paths;
use crate::refs;
use crate::state::StateFile;

pub struct Hub {
    pub paths: Paths,
    /// The socket the host CLI reaches this hub on.
    pub socket: PathBuf,
    /// The stews' socket (the host driver's `STEW_HUB`; the bento channel's host socket).
    pub box_socket: PathBuf,
    /// The dir holding the in-box client as `stew` (host stews get it first on `PATH`).
    pub client: Option<PathBuf>,
    /// Serializes git in the mirror (concurrent fetches fight over its lock files).
    mirror: Mutex<()>,
    /// Guards the transcripts checkout.
    pub transcripts: Mutex<()>,
    pub state: Mutex<StateFile>,
    /// Stews a command is working on.
    busy: Mutex<BTreeSet<String>>,
    /// Requests being handled, and when the last one ended (for `--idle-exit`).
    active: AtomicUsize,
    last: Mutex<Instant>,
}

impl Hub {
    /// Loads the state file and marks creations a previous daemon left running as failed.
    pub fn open(paths: Paths, socket: PathBuf, box_socket: PathBuf) -> Result<Self> {
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
            box_socket,
            client: client_dir(),
            mirror: Mutex::new(()),
            transcripts: Mutex::new(()),
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

    /// A request from the host CLI's socket (`from_box` false) or the stews' (true).
    pub fn handle(&self, req: Request, from_box: bool) -> Reply {
        let res = if from_box && req.stew.is_none() {
            Err(Error::Refused(
                "on the stews' socket a request names its stew (notes/stew.yaml) and its token"
                    .into(),
            ))
        } else {
            self.caller(&req)
        };
        let res = res.and_then(|caller| self.dispatch(req.call, caller.as_deref()));
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
                site: match caller {
                    Some(c) => Some(self.site_for_stew(&config, c, site.as_deref())?),
                    None => site,
                },
                repos,
                title,
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
                // one assembly at a time (they share the temporary file names)
                let _lock = self.mirror_lock();
                crate::skills::assemble(&self.paths.skills())?;
                Ok(Reply::Done { warnings: vec![] })
            }
            Call::Open { id, site } => self.open_stew(&id, site.as_deref()),
            Call::Remove { id, force } => self.remove(&id, force, false),
            Call::Close { id, force } => self.remove(&id, force, true),
            Call::Start { id } => self.start(id.as_deref()),
            Call::Stop { id } => self.stop(&id),
            Call::Title { id, title } => self.title(&id, &title),
            Call::Ls => self.ls(),
            Call::Export { id, push } => self.export(id.as_deref(), push),
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

    /// Where a stew's `new` goes: the default site (a stew names none), and a host site only
    /// for a caller that is on one itself (a box never makes a stew outside a box).
    fn site_for_stew(&self, config: &Config, caller: &str, asked: Option<&str>) -> Result<String> {
        if asked.is_some() {
            return Err(Error::Refused(
                "a stew's `new` goes to the default site (no --site)".into(),
            ));
        }
        let (name, site) = config.site(None)?;
        if site.driver == DriverKind::Host {
            let own = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .state
                .stews
                .get(caller)
                .map(|p| p.site.clone())
                .unwrap_or_default();
            let on_host = config
                .site(Some(&own))
                .is_ok_and(|(_, s)| s.driver == DriverKind::Host);
            if !on_host {
                return Err(Error::Refused(format!(
                    "the default site {name} is a host site: only a stew on a host site may make one there"
                )));
            }
        }
        Ok(name)
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

/// The sockets systemd passed (socket activation: `LISTEN_PID` is us, `LISTEN_FDS` ≥ 1:
/// the CLI's at fd 3, the stews' at fd 4 when there are two), if any. As `sd_listen_fds`
/// does, they are made close-on-exec and the variables are unset, so no child (a
/// host-driver command, bento) inherits the hub's sockets. Call it before any thread starts.
pub fn activated() -> Option<(UnixListener, Option<UnixListener>)> {
    use std::os::fd::FromRawFd;
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds < 1 {
        return None;
    }
    // SAFETY: single-threaded here (see above); systemd hands fd 3 to this process as a
    // listening socket and nothing else in the process owns it.
    unsafe {
        for var in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
            std::env::remove_var(var);
        }
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
        let stews = (fds >= 2).then(|| {
            libc::fcntl(4, libc::F_SETFD, libc::FD_CLOEXEC);
            UnixListener::from_raw_fd(4)
        });
        Some((UnixListener::from_raw_fd(3), stews))
    }
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

/// Serves until killed (or idle for `idle_exit`), a thread per connection, on the CLI's
/// listener and the stews'. The accept loop itself decides the idle exit, only when no
/// connection is waiting on either: one already accepted is always served (connections not
/// yet accepted stay with systemd's sockets). Between accepts it sleeps in `poll` (a second
/// at most), never in a busy loop.
pub fn serve(
    hub: Arc<Hub>,
    cli: UnixListener,
    stews: UnixListener,
    idle_exit: Option<Duration>,
) -> Result<()> {
    eprintln!(
        "stew daemon: listening on {} (stews: {})",
        hub.socket.display(),
        hub.box_socket.display()
    );
    cli.set_nonblocking(true)?;
    stews.set_nonblocking(true)?;
    loop {
        let mut accepted = false;
        for (listener, from_box) in [(&cli, false), (&stews, true)] {
            let conn = match listener.accept() {
                Ok((c, _)) => c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    eprintln!("stew daemon: accept: {e}");
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            accepted = true;
            if let Err(e) = conn.set_nonblocking(false) {
                eprintln!("stew daemon: {e}");
                continue;
            }
            let hub = hub.clone();
            hub.active.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                if let Err(e) = connection(&hub, conn, from_box) {
                    eprintln!("stew daemon: {e}");
                }
                *hub.last.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
                hub.active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        if accepted {
            continue;
        }
        if idle_exit.is_some_and(|idle| hub.idle_for(idle)) {
            eprintln!("stew daemon: idle, exiting");
            return Ok(());
        }
        wait_readable(&[&cli, &stews], Duration::from_secs(1));
    }
}

/// Waits until a connection is waiting on one of `listeners`, at most `limit` (no busy loop).
fn wait_readable(listeners: &[&UnixListener], limit: Duration) {
    use std::os::fd::AsRawFd;
    let mut fds: Vec<libc::pollfd> = listeners
        .iter()
        .map(|l| libc::pollfd {
            fd: l.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: valid pollfds, for the duration of the call
    unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            limit.as_millis() as libc::c_int,
        )
    };
}

fn connection(hub: &Hub, conn: UnixStream, from_box: bool) -> std::io::Result<()> {
    // one line, of bounded size (a longer one reads as cut off)
    let reply = match recv::<Request>(&mut BufReader::new((&conn).take(stew_proto::MAX_REQUEST))) {
        Ok(Some(req)) => hub.handle(req, from_box),
        Ok(None) => return Ok(()),
        Err(e) => Reply::Error {
            kind: stew_proto::ErrorKind::Fail,
            message: format!("bad request: {e}"),
        },
    };
    send(&mut &conn, &reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Placed;
    use stew_proto::ErrorKind;

    /// A stew's `new`: no site of its own choosing, and a host site only from a host stew.
    #[test]
    fn what_site_a_stew_may_make_stews_on() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        fs::create_dir_all(&paths.config).unwrap();
        let hub = Hub::open(paths.clone(), tmp.path().join("s"), tmp.path().join("b")).unwrap();
        for (id, site) in [("boxed-0000", "box"), ("hosted-0000", "here")] {
            hub.state.lock().unwrap().state.stews.insert(
                id.into(),
                Placed {
                    site: site.into(),
                    token: "t".repeat(32),
                    from: None,
                },
            );
        }
        let new = |stew: &str, site: Option<&str>| {
            hub.handle(
                Request {
                    stew: Some(stew.into()),
                    token: Some("t".repeat(32)),
                    call: Call::New {
                        repos: vec![],
                        title: "x".into(),
                        site: site.map(String::from),
                        branch: None,
                        base: None,
                        here: None,
                        from: None,
                        brief: None,
                        start: true,
                        agent: None,
                        model: None,
                    },
                },
                true,
            )
        };
        let refused = |r: Reply, what: &str| match r {
            Reply::Error {
                kind: ErrorKind::Refused,
                message,
            } => assert!(message.contains(what), "{message}"),
            other => panic!("not refused ({what}): {other:?}"),
        };
        let config = |default: &str| {
            fs::write(
                paths.config_file(),
                format!(
                    "site: {default}\nsites:\n  here: {{driver: host}}\n  box: {{driver: bento}}\n"
                ),
            )
            .unwrap()
        };
        config("here");
        refused(new("boxed-0000", None), "host site");
        refused(new("boxed-0000", Some("box")), "default site");
        refused(new("hosted-0000", Some("here")), "default site");
        let c = Config::load(&paths).unwrap();
        assert_eq!(hub.site_for_stew(&c, "hosted-0000", None).unwrap(), "here");
        config("box");
        let c = Config::load(&paths).unwrap();
        assert_eq!(hub.site_for_stew(&c, "boxed-0000", None).unwrap(), "box");
        assert_eq!(hub.site_for_stew(&c, "hosted-0000", None).unwrap(), "box");
    }
}
