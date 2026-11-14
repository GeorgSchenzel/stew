//! The hub daemon: one request per connection, JSON lines (stew-proto). It never touches a
//! stew dir; it reaches remotes with `git` and (later) sites with `bento`.

use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fs, thread};

use stew_proto::{Call, Reply, Request, VERSION, recv, send};

use crate::catalog::Catalog;
use crate::config::Config;
use crate::error::{Result, fail};
use crate::mirror::Mirror;
use crate::paths::Paths;
use crate::refs;
use crate::state::StateFile;

pub struct Hub {
    pub paths: Paths,
    /// Serializes git in the mirror (concurrent fetches fight over its lock files).
    mirror: Mutex<()>,
    pub state: Mutex<StateFile>,
}

impl Hub {
    /// Loads the state file and marks creations a previous daemon left running as failed.
    pub fn open(paths: Paths) -> Result<Self> {
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
            mirror: Mutex::new(()),
            state: Mutex::new(state),
        })
    }

    pub fn handle(&self, req: Request) -> Reply {
        match self.dispatch(req.call) {
            Ok(r) => r,
            Err(e) => Reply::Error {
                kind: e.kind(),
                message: e.to_string(),
            },
        }
    }

    fn dispatch(&self, call: Call) -> Result<Reply> {
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
            Call::LsAll { search } => {
                let mirror = self.mirror(&config)?;
                let _lock = self.mirror.lock().unwrap_or_else(|p| p.into_inner());
                mirror.ensure()?;
                let mut warnings = Vec::new();
                // an unreachable remote is normal: list what the mirror has
                if let Err(e) = mirror.fetch() {
                    warnings.push(format!("{e} (showing the last fetched state)"));
                }
                let (stews, more) = mirror.tracked(search.as_deref())?;
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

/// Serves until killed, a thread per connection.
pub fn serve(hub: Arc<Hub>, listener: UnixListener, socket: PathBuf) -> Result<()> {
    eprintln!("stew daemon: listening on {}", socket.display());
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("stew daemon: accept: {e}");
                continue;
            }
        };
        let hub = hub.clone();
        thread::spawn(move || {
            if let Err(e) = connection(&hub, conn) {
                eprintln!("stew daemon: {e}");
            }
        });
    }
    Ok(())
}

fn connection(hub: &Hub, conn: UnixStream) -> std::io::Result<()> {
    let reply = match recv::<Request>(&mut BufReader::new(&conn)) {
        Ok(Some(req)) => hub.handle(req),
        Ok(None) => return Ok(()),
        Err(e) => Reply::Error {
            kind: stew_proto::ErrorKind::Fail,
            message: format!("bad request: {e}"),
        },
    };
    send(&mut &conn, &reply)
}
