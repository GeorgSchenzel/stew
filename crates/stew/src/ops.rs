//! The hub's commands on stews: new, open, remove, close, start, stop, title, ls, and where
//! `exec`/`enter` run. The daemon reaches a stew only through its site's driver; all git in
//! clones and `notes/` runs where the stew lives, through `exec` (`inspect`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use stew_proto::{
    AgentInfo, CreationInfo, Here, Listed, LiveRepo, Openness, Reply, RepoRecord, Resolved,
    StewMeta, Tracked,
};

use crate::catalog::Catalog;
use crate::config::Config;
use crate::daemon::{Hub, progress};
use crate::driver::{self, BoxState, Driver, ExecOpts, Target};
use crate::error::{Error, Result, fail};
use crate::git::git;
use crate::inspect::{self, Look, Looked, Mode};
use crate::mirror::{meta_yaml, parse_meta};
use crate::runtime::{self, Runtime};
use crate::state::{Creation, Placed, Progress};
use crate::transcripts::{self, Exported, Transcripts};
use crate::{agent, id, refs};

/// How long `info` waits for a look at its clones.
const LS_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the pushed check may take (it fetches every remote of every clone; a stalled
/// connection fails sooner, inspect.sh sets low-speed limits).
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long stopping a stew's own runtime core may take at `remove`.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Marks a stew busy for one command; another command on it at once is refused.
struct Busy<'a> {
    hub: &'a Hub,
    id: String,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.hub.busy_ids().remove(&self.id);
    }
}

fn target(id: &str, token: &str) -> Target {
    Target {
        id: id.to_string(),
        env: vec![("STEW_TOKEN".into(), token.to_string())],
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub struct NewArgs {
    pub repos: Vec<String>,
    pub title: String,
    pub site: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub here: Option<Here>,
    pub from: Option<String>,
    pub brief: Option<String>,
    pub start: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub account: Option<String>,
}

impl Hub {
    fn busy(&self, id: &str) -> Result<Busy<'_>> {
        if !self.busy_ids().insert(id.to_string()) {
            return Err(fail!("stew {id} is busy with another command"));
        }
        Ok(Busy {
            hub: self,
            id: id.to_string(),
        })
    }

    fn driver(&self, config: &Config, site: Option<&str>) -> Result<(String, Box<dyn Driver>)> {
        let (name, site) = config.site(site)?;
        let hub = self.box_socket.to_string_lossy();
        Ok((
            name,
            driver::for_site(&site, &self.paths, &hub, self.client.as_deref()),
        ))
    }

    /// A stew that exists on a site: its record and its site's driver.
    fn placed(&self, config: &Config, id: &str) -> Result<(Placed, Box<dyn Driver>)> {
        id::check(id)?;
        let placed = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .get(id)
            .cloned()
            .ok_or_else(|| fail!("stew {id} is on no site (`stew open {id}` brings it back)"))?;
        let (_, driver) = self.driver(config, Some(&placed.site))?;
        Ok((placed, driver))
    }

    fn save_state(&self, f: impl FnOnce(&mut crate::state::State)) -> Result<()> {
        let mut file = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut file.state);
        file.save()
    }

    fn artifacts(&self, config: &Config) -> Result<String> {
        crate::url::normalize(config.artifacts(&self.paths)?)
    }

    /// The tracked stews from the mirror, after a fetch (a failed fetch is a warning).
    fn mirror_every(&self, config: &Config, warnings: &mut Vec<String>) -> Result<Vec<Tracked>> {
        let mirror = self.mirror(config)?;
        let _lock = self.mirror_lock();
        mirror.ensure()?;
        if let Err(e) = mirror.fetch() {
            warnings.push(format!("{e} (using the last fetched state)"));
        }
        let (all, more) = mirror.every()?;
        warnings.extend(more);
        Ok(all)
    }

    /// The tracked stews `ls` shows, from the mirror as last fetched (the daemon fetches
    /// in the background and after its own pushes): the open ones, with `closed` the closed
    /// ones too; `search` keeps those whose id, title or notes contain it. A mirror that
    /// can't be read is a warning and no rows.
    fn mirror_cached(
        &self,
        config: &Config,
        search: Option<&str>,
        closed: bool,
        warnings: &mut Vec<String>,
    ) -> Vec<Tracked> {
        let read = || {
            let mirror = self.mirror(config)?;
            // reads go without the lock (a fetch holding it would hold up `ls`; git
            // updates refs atomically), only the first clone takes it
            if !self.paths.mirror().join("HEAD").exists() {
                let _lock = self.mirror_lock();
                mirror.ensure()?;
            }
            mirror.tracked(search, closed)
        };
        match read() {
            Ok((all, more)) => {
                warnings.extend(more);
                all
            }
            Err(e) => {
                warnings.push(e.to_string());
                vec![]
            }
        }
    }

    pub fn new_stew(&self, a: NewArgs) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let title = a.title.trim().to_string();
        if title.is_empty() {
            return Err(fail!("a stew needs a title"));
        }
        if let Some(f) = &a.from {
            id::check(f)?;
        }
        if a.start && a.brief.is_none() {
            return Err(fail!("--start needs --brief"));
        }
        if (a.agent.is_some() || a.model.is_some() || a.account.is_some()) && !a.start {
            return Err(fail!("--agent, --model and --account go with --start"));
        }
        if a.brief
            .as_ref()
            .is_some_and(|b| b.len() > stew_proto::MAX_BRIEF)
        {
            return Err(fail!(
                "the brief is longer than {} bytes",
                stew_proto::MAX_BRIEF
            ));
        }
        // the agent's command line, checked before anything exists
        let launch = if a.start {
            runtime::can_run(&config)?;
            let choice = agent::choose(
                &config,
                a.agent.as_deref(),
                a.model.as_deref(),
                a.account.as_deref(),
            );
            Some((choice.prefix(), agent::command(&choice, agent::BRIEF_TASK)?))
        } else {
            None
        };
        let rt = runtime::for_config(&config);
        let artifacts = self.artifacts(&config)?;
        let (site, driver) = self.driver(&config, a.site.as_deref())?;
        let feature = match a.branch.as_deref() {
            None => None,
            Some("") => Some(id::slug(&title)),
            Some(b) => Some(b.to_string()),
        };
        if let Some(b) = &feature {
            git(Path::new("/"), ["check-ref-format", "--branch", b])
                .map_err(|_| fail!("'{b}' is not a valid branch name"))?;
        }
        let cat = Catalog::load(&self.paths, &config)?;
        let entries = a
            .repos
            .iter()
            .map(|r| cat.find(r))
            .collect::<Result<Vec<_>>>()?;
        let (resolved, mut warnings) =
            refs::resolve_new(&entries, a.base.as_deref(), a.here.as_ref())?;

        let meta = StewMeta {
            id: String::new(),
            title,
            from: a.from,
            created: id::today(),
            state: None,
            closed: None,
            repos: records(&resolved),
        };
        let id = {
            let mirror = self.mirror(&config)?;
            let _lock = self.mirror_lock();
            mirror.claim(&meta, a.brief.as_deref())?
        };
        progress(&format!("{id}: claimed stew/{id}"));
        let token = id::token()?;
        self.create(
            &id,
            &token,
            &site,
            driver.as_ref(),
            rt.as_ref(),
            &artifacts,
            &resolved,
            feature.as_deref(),
            &mut warnings,
            None,
            meta.from.as_deref(),
        )?;
        let agent = match launch {
            None => None,
            Some((name, argv)) => {
                progress(&format!("{id}: starting {name} on the brief"));
                let t = target(&id, &token);
                let address = driver
                    .root(&t)
                    .and_then(|root| rt.run(&id, &name, &argv, &root))
                    .map_err(|e| fail!("stew {id} exists, but its agent did not start: {e}"))?;
                Some(address)
            }
        };
        Ok(Reply::Created {
            id,
            agent,
            warnings,
        })
    }

    pub fn open_stew(&self, id: &str, site: Option<&str>) -> Result<Reply> {
        id::check(id)?;
        let config = Config::load(&self.paths)?;
        if let Some(p) = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .get(id)
        {
            return Err(fail!("stew {id} is already on site {}", p.site));
        }
        let artifacts = self.artifacts(&config)?;
        let (site, driver) = self.driver(&config, site)?;
        let mut warnings = Vec::new();
        let meta = self
            .mirror_every(&config, &mut warnings)?
            .into_iter()
            .find(|t| t.meta.id == id)
            .map(|t| t.meta)
            .ok_or_else(|| fail!("no tracked stew {id}"))?;
        let (resolved, more) = refs::resolve_open(&meta.repos)?;
        warnings.extend(more);
        // the branch is box-writable: `from` (what lets a stew start agents here) is the
        // claim commit's, never what was pushed onto the branch while it was closed
        let from = {
            let mirror = self.mirror(&config)?;
            let _lock = self.mirror_lock();
            mirror.claimed_from(id)?
        };
        let reopen = meta.is_closed().then(|| StewMeta {
            state: Some(Openness::Open),
            closed: None,
            from: from.clone(),
            ..meta.clone()
        });
        let token = id::token()?;
        let rt = runtime::for_config(&config);
        self.create(
            id,
            &token,
            &site,
            driver.as_ref(),
            rt.as_ref(),
            &artifacts,
            &resolved,
            None,
            &mut warnings,
            reopen,
            from.as_deref(),
        )?;
        Ok(Reply::Created {
            id: id.to_string(),
            agent: None,
            warnings,
        })
    }

    /// The driver's create, clones through `exec`, (reopen: the commit), the driver's
    /// start (a restart when the clones' ports are pending), the runtime's connect. A failure up to the start removes the stew from the
    /// site and records the creation as failed; one to connect is a warning (`stew start`
    /// connects again).
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        id: &str,
        token: &str,
        site: &str,
        driver: &dyn Driver,
        rt: &dyn Runtime,
        artifacts: &str,
        resolved: &[Resolved],
        feature: Option<&str>,
        warnings: &mut Vec<String>,
        reopen: Option<StewMeta>,
        from: Option<&str>,
    ) -> Result<()> {
        let _busy = self.busy(id)?;
        if let Some(p) = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .get(id)
        {
            return Err(fail!("stew {id} is already on site {}", p.site));
        }
        self.save_state(|s| {
            s.creations.insert(
                id.to_string(),
                Creation {
                    state: Progress::Running,
                    started: now(),
                    error: None,
                },
            );
        })?;
        let t = target(id, token);
        progress(&format!("{id}: creating it on {site}"));
        // what create refuses (e.g. a box of that name exists) was not made here: keep it
        if let Err(e) = driver.create(&t) {
            self.fail_creation(id, &e.to_string());
            return Err(fail!("creating stew {id} failed: {e}"));
        }
        let made = (|| -> Result<Vec<String>> {
            let mut args = vec![
                artifacts.to_string(),
                id.to_string(),
                feature.unwrap_or("").to_string(),
            ];
            for r in resolved {
                args.extend([r.dir.clone(), r.remote.clone(), r.branch.clone()]);
            }
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let dirs: Vec<&str> = resolved.iter().map(|r| r.dir.as_str()).collect();
            progress(format!("{id}: cloning notes {}", dirs.join(" ")).trim_end());
            let out = driver.exec(
                &t,
                &inspect::sh(inspect::CLONE, &args),
                None,
                None,
                false,
                None,
            )?;
            if !out.ok() {
                return Err(fail!("cloning failed: {}", out.reason()));
            }
            if let Some(meta) = &reopen {
                commit(
                    driver,
                    &t,
                    meta,
                    "stew: reopened",
                    &format!("- {} reopened", id::today()),
                )
                .map_err(|e| fail!("recording the reopen failed: {e}"))?;
            }
            // the clones' ports apply only at a boot (bento): one restart, before any agent
            let mut w = Vec::new();
            let pending = match driver.ports(&t) {
                Ok(ports) => ports.iter().any(|p| p.pending),
                Err(e) => {
                    w.push(format!("the clones' ports: {e}"));
                    false
                }
            };
            w.extend(if pending {
                progress(&format!(
                    "{id}: restarting (its clones' ports apply at boot)"
                ));
                driver.restart(&t)?
            } else {
                progress(&format!("{id}: starting"));
                driver.start(&t)?
            });
            Ok(w)
        })();
        match made {
            Ok(more) => {
                warnings.extend(more);
                self.save_state(|s| {
                    s.creations.remove(id);
                    s.stews.insert(
                        id.to_string(),
                        Placed {
                            site: site.to_string(),
                            token: token.to_string(),
                            from: from.map(String::from),
                        },
                    );
                })?;
                connect(rt, driver, id, warnings);
                Ok(())
            }
            Err(e) => {
                let mut msg = e.to_string();
                // ours, just made: a broken one goes too
                if let Err(e2) = driver.remove(&t, true) {
                    msg.push_str(&format!("; removing what was made failed too: {e2}"));
                }
                self.fail_creation(id, &msg);
                Err(fail!("creating stew {id} failed: {msg}"))
            }
        }
    }

    fn fail_creation(&self, id: &str, msg: &str) {
        let _ = self.save_state(|s| {
            let started = s.creations.get(id).map_or_else(now, |c| c.started);
            s.creations.insert(
                id.to_string(),
                Creation {
                    state: Progress::Failed,
                    started,
                    error: Some(msg.to_string()),
                },
            );
        });
    }

    /// `remove`, and with `close` also `state: closed` and a LOG.md line.
    pub fn remove(&self, id: &str, force: bool, close: bool) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let _busy = self.busy(id)?;
        let t = target(id, &placed.token);
        let mut warnings = Vec::new();
        let rt = runtime::for_config(&config);

        // a first check: a refusal here changes nothing
        progress(&format!("{id}: checking that everything is pushed"));
        let look = verify(driver.as_ref(), &t, force, "", &mut warnings)?;
        let undo = |warnings: &mut Vec<String>, e: Error| -> Error {
            connect(rt.as_ref(), driver.as_ref(), id, warnings);
            e
        };
        // its agents stop, so nothing in the runtime writes into it from here on
        progress(&format!("{id}: stopping its agents"));
        if let Err(e) = disconnect(rt.as_ref(), driver.as_ref(), &t, &mut warnings) {
            if !force {
                return Err(undo(
                    &mut warnings,
                    Error::Refused(format!(
                        "{e}, nothing removed (--force removes anyway; its agents may still run)"
                    )),
                ));
            }
            warnings.push(format!("{e} (--force)"));
        }

        // its sessions, now that its agents are stopped (fail-closed like the check: what
        // isn't exported is lost with it)
        progress(&format!("{id}: exporting its sessions"));
        match self.export_sessions(&config, driver.as_ref(), &t, false, &mut warnings) {
            Ok(()) => self.publish(&config, true, &mut warnings),
            Err(e) if force => warnings.push(format!("sessions not exported (--force): {e}")),
            Err(e) => {
                return Err(undo(
                    &mut warnings,
                    Error::Refused(format!(
                        "exporting {id}'s sessions failed, nothing removed (its agents were stopped; --force removes anyway): {e}"
                    )),
                ));
            }
        }

        // checked again right before it goes: what was done since the first check (an agent
        // finishing as it stopped, a shell still open) is not lost silently
        let look = match look {
            Some(first) => {
                progress(&format!("{id}: checking again"));
                let again = verify(
                    driver.as_ref(),
                    &t,
                    force,
                    " (since the first check; its agents were stopped)",
                    &mut warnings,
                );
                match again {
                    Ok(l) => Some(l.unwrap_or(first)),
                    Err(e) => return Err(undo(&mut warnings, e)),
                }
            }
            None => None,
        };

        // the snapshot (and for close, the state and the LOG line), committed and pushed only
        // now: a refusal above leaves the stew as it was, not marked closed
        match &look {
            Some(l) => {
                let res = self.snapshot(&config, driver.as_ref(), &t, l, close, &mut warnings);
                match res {
                    Ok(()) => {}
                    Err(e) if force => {
                        warnings.push(format!("no snapshot (--force): {e}"));
                        if close {
                            warnings.push(format!("{id} is not marked closed"));
                        }
                    }
                    Err(e) => {
                        return Err(undo(
                            &mut warnings,
                            fail!(
                                "writing the snapshot failed, nothing removed (its agents were stopped): {e}"
                            ),
                        ));
                    }
                }
            }
            None if close => warnings.push(format!("{id} is not marked closed: no snapshot")),
            None => {}
        }
        progress(&format!("{id}: removing it from {}", placed.site));
        driver.remove(&t, force)?;
        self.save_state(|s| {
            s.stews.remove(id);
        })?;
        Ok(Reply::Done { warnings })
    }

    /// Writes `stew.yaml` from what is in the root and pushes it (`close`: closed, a LOG
    /// line).
    fn snapshot(
        &self,
        config: &Config,
        driver: &dyn Driver,
        t: &Target,
        look: &Look,
        close: bool,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let mut meta = self.current_meta(config, look, &t.id, warnings)?;
        let cat = Catalog::load(&self.paths, config).ok();
        let (repos, more) = inspect::snapshot(&look.repos, &meta.repos, cat.as_ref());
        warnings.extend(more);
        meta.repos = repos;
        let (message, line) = if close {
            meta.state = Some(Openness::Closed);
            meta.closed = Some(id::today());
            (
                "stew: closed".to_string(),
                format!("- {} closed", id::today()),
            )
        } else {
            ("stew: snapshot".to_string(), String::new())
        };
        commit(driver, t, &meta, &message, &line)
    }

    /// `stew.yaml` as the stew has it, else as the notes branch has it.
    fn current_meta(
        &self,
        config: &Config,
        look: &Look,
        id: &str,
        warnings: &mut Vec<String>,
    ) -> Result<StewMeta> {
        if let Some(text) = &look.meta {
            match parse_meta(text) {
                Ok(m) if m.id == id => return Ok(m),
                Ok(m) => warnings.push(format!(
                    "notes/stew.yaml names {}, not {id}; using the notes branch",
                    m.id
                )),
                Err(e) => warnings.push(format!("notes/{e}; using the notes branch")),
            }
        }
        self.mirror_every(config, warnings)?
            .into_iter()
            .find(|t| t.meta.id == id)
            .map(|t| t.meta)
            .ok_or_else(|| fail!("no stew.yaml for {id}, neither in the stew nor tracked"))
    }

    pub fn start(&self, id: Option<&str>) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let ids: Vec<String> = match id {
            Some(i) => vec![i.to_string()],
            None => self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .state
                .stews
                .keys()
                .cloned()
                .collect(),
        };
        let mut warnings = Vec::new();
        let mut failed = Vec::new();
        for i in &ids {
            let res = (|| {
                let (placed, driver) = self.placed(&config, i)?;
                let _busy = self.busy(i)?;
                progress(&format!("{i}: starting on {}", placed.site));
                let mut w = driver.start(&target(i, &placed.token))?;
                connect(
                    runtime::for_config(&config).as_ref(),
                    driver.as_ref(),
                    i,
                    &mut w,
                );
                Ok(w)
            })();
            match res {
                Ok(w) => warnings.extend(w),
                Err(e) if id.is_some() => return Err(e),
                Err(e) => failed.push(format!("{i}: {e}")),
            }
        }
        if !failed.is_empty() {
            return Err(fail!("could not start: {}", failed.join("; ")));
        }
        Ok(Reply::Done { warnings })
    }

    pub fn stop(&self, id: &str) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let _busy = self.busy(id)?;
        let t = target(id, &placed.token);
        let mut warnings = Vec::new();
        // a stopped stew changed nothing since its last stop: nothing to snapshot
        progress(&format!("{id}: checking what is pushed"));
        match inspect::look(driver.as_ref(), &t, Mode::Check, true, Some(CHECK_TIMEOUT)) {
            Ok(Looked::NotRunning) => {}
            Ok(Looked::Look(l)) => {
                warnings.extend(l.warnings.iter().cloned());
                if !l.findings.is_empty() {
                    warnings.push(format!(
                        "not everything in {id} is pushed:\n  {}",
                        l.findings.join("\n  ")
                    ));
                }
                // its sessions, before it stops
                progress(&format!(
                    "{id}: exporting its sessions, writing the snapshot"
                ));
                if let Err(e) =
                    self.export_sessions(&config, driver.as_ref(), &t, true, &mut warnings)
                {
                    warnings.push(format!("sessions not exported: {e}"));
                }
                self.publish(&config, true, &mut warnings);
                if let Err(e) =
                    self.snapshot(&config, driver.as_ref(), &t, &l, false, &mut warnings)
                {
                    warnings.push(format!("no snapshot: {e}"));
                }
            }
            Err(e) => warnings.push(format!("no snapshot: {e}")),
        }
        progress(&format!("{id}: stopping"));
        warnings.extend(driver.stop(&t)?);
        // a stopped stew is off the runtime: no via retrying into a box that is off; `start`
        // connects it again (its spaces and tabs live in its own core and come back). One
        // still running (the host driver stops nothing) stays.
        if !matches!(driver.state(&t), Ok(BoxState::Running))
            && let Err(e) = runtime::for_config(&config).disconnect(id)
        {
            warnings.push(format!("disconnecting {id} from the runtime: {e}"));
        }
        Ok(Reply::Done { warnings })
    }

    pub fn title(&self, id: &str, title: &str) -> Result<Reply> {
        let title = title.trim();
        if title.is_empty() {
            return Err(fail!("a stew needs a title"));
        }
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let _busy = self.busy(id)?;
        let t = target(id, &placed.token);
        let mut warnings = Vec::new();
        let look = match inspect::look(driver.as_ref(), &t, Mode::Ls, false, None)? {
            Looked::Look(l) => l,
            Looked::NotRunning => return Err(fail!("stew {id} did not start")),
        };
        let mut meta = self.current_meta(&config, &look, id, &mut warnings)?;
        meta.title = title.to_string();
        commit(
            driver.as_ref(),
            &t,
            &meta,
            &format!("stew: title {title}"),
            "",
        )?;
        Ok(Reply::Done { warnings })
    }

    /// Where `argv` (empty: a shell) runs; the client runs it.
    pub fn launch(&self, id: &str, argv: &[String], no_start: bool, tty: bool) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let t = target(id, &placed.token);
        let l = if argv.is_empty() {
            driver.enter(&t)?
        } else {
            driver.launch(
                &t,
                argv,
                &ExecOpts {
                    no_start,
                    tty,
                    stdin: true,
                    cwd: None,
                },
            )?
        };
        Ok(launch_reply(l))
    }

    pub fn ls(&self, search: Option<&str>, closed: bool) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let mut warnings = Vec::new();
        let tracked = self.mirror_cached(&config, search, closed, &mut warnings);
        let (placed, creations) = {
            let f = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (f.state.stews.clone(), f.state.creations.clone())
        };
        // a stew on a site the mirror doesn't show (not fetched yet, or unreadable) is
        // listed all the same, unless a search or a closed record left it out
        let shown: BTreeSet<&str> = tracked.iter().map(|t| t.meta.id.as_str()).collect();
        let hidden = |id: &str| {
            search.is_some_and(|s| !s.trim().is_empty()) || self.is_tracked_closed(&config, id)
        };
        let extra: Vec<&String> = placed
            .keys()
            .filter(|id| !shown.contains(id.as_str()) && !hidden(id))
            .collect();
        let rows: Vec<(&str, Option<&Tracked>)> = tracked
            .iter()
            .map(|t| (t.meta.id.as_str(), Some(t)))
            .chain(extra.iter().map(|id| (id.as_str(), None)))
            .collect();
        let stews = self.listed(&config, &rows, &placed, &mut warnings);
        let creations = creations
            .into_iter()
            .map(|(id, c)| CreationInfo {
                id,
                state: match c.state {
                    Progress::Running => "running".into(),
                    Progress::Failed => "failed".into(),
                },
                started: c.started,
                error: c.error,
            })
            .collect();
        Ok(Reply::Listed {
            stews,
            creations,
            warnings,
        })
    }

    /// Whether the mirror (as last fetched) has `id` closed.
    fn is_tracked_closed(&self, config: &Config, id: &str) -> bool {
        let Ok(mirror) = self.mirror(config) else {
            return false;
        };
        mirror
            .every()
            .map(|(all, _)| all.iter().any(|t| t.meta.id == id && t.meta.is_closed()))
            .unwrap_or(false)
    }

    /// The rows for `ls`: open stews first, then closed ones, each most recent first. A
    /// stew on a site shows its driver's state (asked in parallel, never starting it),
    /// one on none `removed` or `closed`.
    fn listed(
        &self,
        config: &Config,
        rows: &[(&str, Option<&Tracked>)],
        placed: &BTreeMap<String, Placed>,
        warnings: &mut Vec<String>,
    ) -> Vec<Listed> {
        let agents = self.agents(config, warnings);
        let mut stews: Vec<Listed> = std::thread::scope(|s| {
            let jobs: Vec<_> = rows
                .iter()
                .map(|&(id, t)| {
                    let p = placed.get(id);
                    let agents = agents.get(id).cloned().unwrap_or_default();
                    s.spawn(move || {
                        let mut row = listed_row(id, t, p.map(|p| p.site.as_str()));
                        if let Some(p) = p {
                            row.state = self.state_word(config, id, p);
                            row.agents = agents;
                        }
                        row
                    })
                })
                .collect();
            jobs.into_iter()
                .zip(rows)
                .map(|(j, &(id, t))| {
                    // a look that panicked shows as unreachable, never as nothing
                    j.join().unwrap_or_else(|_| {
                        let p = placed.get(id).map(|p| p.site.as_str());
                        let mut row = listed_row(id, t, p);
                        row.state = "unreachable".into();
                        row
                    })
                })
                .collect()
        });
        stews.sort_by(|a, b| {
            (a.state == "closed")
                .cmp(&(b.state == "closed"))
                .then(b.updated.cmp(&a.updated))
                .then(a.id.cmp(&b.id))
        });
        stews
    }

    /// The driver's word for a stew on a site: `running`, `stopped`, ...; `unreachable`
    /// when the driver can't say.
    fn state_word(&self, config: &Config, id: &str, p: &Placed) -> String {
        let Ok((_, driver)) = self.driver(config, Some(&p.site)) else {
            return "unreachable".into();
        };
        match driver.state(&target(id, &p.token)) {
            Ok(s) => s.word().to_string(),
            Err(_) => "unreachable".into(),
        }
    }

    /// The runtime's agents by stew; a runtime that can't be asked is a warning.
    fn agents(
        &self,
        config: &Config,
        warnings: &mut Vec<String>,
    ) -> BTreeMap<String, Vec<AgentInfo>> {
        runtime::for_config(config).agents().unwrap_or_else(|e| {
            warnings.push(format!("agents unknown: {e}"));
            BTreeMap::new()
        })
    }
}

impl Hub {
    /// The caller's own stew: its row, and its clones looked at live.
    pub fn info(&self, id: &str) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let mut warnings = Vec::new();
        let tracked = self
            .mirror_cached(&config, None, true, &mut warnings)
            .into_iter()
            .find(|t| t.meta.id == id);
        let stew = self
            .listed(
                &config,
                &[(id, tracked.as_ref())],
                &BTreeMap::from([(id.to_string(), placed.clone())]),
                &mut warnings,
            )
            .pop()
            .ok_or_else(|| fail!("stew {id} is not listed"))?;
        let t = target(id, &placed.token);
        let repos = match inspect::look(driver.as_ref(), &t, Mode::Ls, true, Some(LS_TIMEOUT)) {
            Ok(Looked::Look(l)) => l
                .repos
                .iter()
                .map(|f| LiveRepo {
                    dir: f.dir.clone(),
                    branch: f.branch.clone(),
                })
                .collect(),
            _ => vec![],
        };
        let root = driver.root(&t)?;
        Ok(Reply::Info {
            stew,
            repos,
            driver: placed_driver_word(&config, &placed.site),
            root,
        })
    }

    /// An agent in `id` on `task`. A stew's client may start one only in a stew it started
    /// (or in itself). Actions start stews: a stopped one is started, and connected.
    pub fn agent(
        &self,
        id: &str,
        agent: Option<&str>,
        model: Option<&str>,
        account: Option<&str>,
        task: &str,
        caller: Option<&str>,
    ) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let choice = agent::choose(&config, agent, model, account);
        let argv = agent::command(&choice, task)?;
        let agent = choice.prefix();
        runtime::can_run(&config)?;
        let (placed, driver) = self.placed(&config, id)?;
        let mut warnings = Vec::new();
        // the daemon's own record, never a branch's stew.yaml (any stew can push those)
        if let Some(c) = caller.filter(|c| *c != id)
            && placed.from.as_deref() != Some(c)
        {
            return Err(Error::Refused(format!(
                "{id} was not started from {c}: agents go only into stews you started"
            )));
        }
        let t = target(id, &placed.token);
        let rt = runtime::for_config(&config);
        {
            // starting and connecting are one command at a time; `hc run` is not (hc names
            // concurrent agents apart), so two agents can start in one stew at once
            let _busy = self.busy(id)?;
            progress(&format!("{id}: starting (when stopped) and connecting"));
            warnings.extend(driver.start(&t)?);
            rt.connect(id, &runtime::via(id), &driver.root(&t)?)?;
        }
        progress(&format!("{id}: starting {agent}"));
        let address = rt.run(id, &agent, &argv, &driver.root(&t)?)?;
        Ok(Reply::Started { address, warnings })
    }

    /// Where `stew attach` runs: an agent's address (`<id>/<name>`), or a name that only
    /// one stew's agents have.
    pub fn attach(&self, agent: &str) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let ids: Vec<String> = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .keys()
            .cloned()
            .collect();
        let rt = runtime::for_config(&config);
        let all = rt.agents()?;
        let found: Vec<&AgentInfo> = ids
            .iter()
            .flat_map(|i| all.get(i).into_iter().flatten())
            .filter(|a| {
                a.address == agent
                    || (!agent.contains('/')
                        && a.address.rsplit_once('/').map(|(_, n)| n) == Some(agent))
            })
            .collect();
        match found.as_slice() {
            [one] => Ok(launch_reply(rt.attach(&one.address)?)),
            [] => Err(fail!("no agent {agent} in a stew (`stew ls` lists them)")),
            many => Err(fail!(
                "{agent} is in several stews: {}",
                many.iter()
                    .map(|a| a.address.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The tracked stews started from `id`, open and closed.
    pub fn ls_from(&self, id: &str) -> Result<Reply> {
        id::check(id)?;
        let config = Config::load(&self.paths)?;
        let mut warnings = Vec::new();
        let placed = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .clone();
        let children: Vec<Tracked> = self
            .mirror_cached(&config, None, true, &mut warnings)
            .into_iter()
            .filter(|t| t.meta.from.as_deref() == Some(id))
            .collect();
        let rows: Vec<(&str, Option<&Tracked>)> = children
            .iter()
            .map(|t| (t.meta.id.as_str(), Some(t)))
            .collect();
        let stews = self.listed(&config, &rows, &placed, &mut warnings);
        Ok(Reply::Listed {
            stews,
            creations: vec![],
            warnings,
        })
    }
}

impl Hub {
    /// The transcripts repo, if the config names one.
    fn transcripts_repo(&self, config: &Config) -> Result<Option<Transcripts>> {
        match &config.transcripts {
            None => Ok(None),
            Some(r) => Ok(Some(Transcripts::new(
                self.paths.transcripts(),
                crate::url::normalize(r)?,
            ))),
        }
    }

    /// The checkout, ready (made on first use).
    fn transcripts_ready(
        &self,
        config: &Config,
        warnings: &mut Vec<String>,
    ) -> Result<Option<Transcripts>> {
        let Some(tr) = self.transcripts_repo(config)? else {
            return Ok(None);
        };
        let _lock = self.transcripts.lock().unwrap_or_else(|p| p.into_inner());
        tr.ensure(warnings)?;
        Ok(Some(tr))
    }

    /// One stew's sessions into the checkout, for `stop` and `remove`: an error when the
    /// export failed or is incomplete. No transcripts repo configured: a warning.
    fn export_sessions(
        &self,
        config: &Config,
        driver: &dyn Driver,
        t: &Target,
        no_start: bool,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let Some(tr) = self.transcripts_ready(config, warnings)? else {
            warnings.push(format!(
                "no transcripts repo (`transcripts:` in {}): {}'s sessions are not exported",
                self.paths.config_file().display(),
                t.id
            ));
            return Ok(());
        };
        match transcripts::export(&tr, &self.transcripts, driver, t, no_start)? {
            Exported::NotRunning => Err(fail!("{} is not running", t.id)),
            Exported::Done(r) if r.problems.is_empty() => Ok(()),
            Exported::Done(r) => Err(fail!("incomplete: {}", r.problems.join("; "))),
        }
    }

    /// Commits the checkout (`now`, else daily) and pushes what the remote lacks; failures
    /// are warnings (the commits stay, the next run pushes them).
    fn publish(&self, config: &Config, now: bool, warnings: &mut Vec<String>) {
        let Ok(Some(tr)) = self.transcripts_repo(config) else {
            return;
        };
        let _lock = self.transcripts.lock().unwrap_or_else(|p| p.into_inner());
        if !tr.dir.join(".git").is_dir() {
            return;
        }
        if let Err(e) = tr.commit(now, &id::today()) {
            warnings.push(format!("transcripts not committed: {e}"));
        }
        if let Err(e) = tr.push() {
            warnings.push(format!(
                "transcripts not pushed: {e} (the next export tries again)"
            ));
        }
    }

    /// `stew export`: the sessions of running stews (stopped ones changed nothing since
    /// their stop: skipped, as are unreachable and busy ones), then the daily commit (or
    /// one now with `push`) and the push.
    pub fn export(&self, id: Option<&str>, push: bool) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let mut warnings = Vec::new();
        let Some(tr) = self.transcripts_ready(&config, &mut warnings)? else {
            return Err(fail!(
                "no transcripts repo: set `transcripts:` in {}",
                self.paths.config_file().display()
            ));
        };
        let ids: Vec<String> = match id {
            Some(i) => {
                id::check(i)?;
                vec![i.to_string()]
            }
            None => self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .state
                .stews
                .keys()
                .cloned()
                .collect(),
        };
        let mut lines = Vec::new();
        let mut failed = Vec::new();
        for i in &ids {
            let res = (|| -> Result<String> {
                let (placed, driver) = self.placed(&config, i)?;
                let _busy = self.busy(i)?;
                let t = target(i, &placed.token);
                Ok(
                    match transcripts::export(&tr, &self.transcripts, driver.as_ref(), &t, true)? {
                        Exported::NotRunning => format!("{i}: not running, skipped"),
                        Exported::Done(r) => {
                            for p in &r.problems {
                                warnings.push(format!("{i}: {p}"));
                            }
                            format!(
                                "{i}: {} session(s), {} file(s) written{}",
                                r.sessions,
                                r.written,
                                if r.problems.is_empty() {
                                    ""
                                } else {
                                    " (incomplete)"
                                }
                            )
                        }
                    },
                )
            })();
            match res {
                Ok(l) => lines.push(l),
                Err(e) if id.is_some() => return Err(e),
                Err(e) => {
                    lines.push(format!("{i}: skipped"));
                    failed.push(format!("{i}: {e}"));
                }
            }
        }
        warnings.extend(failed);
        self.publish(&config, push, &mut warnings);
        Ok(Reply::Exported { lines, warnings })
    }
}

/// A stew as its notes branch has it (the state a word for where it is).
/// A stew's row from its tracking record: on `site`, or else `removed`/`closed`.
fn listed_row(id: &str, tracked: Option<&Tracked>, site: Option<&str>) -> Listed {
    let closed = tracked.filter(|t| t.meta.is_closed());
    Listed {
        id: id.to_string(),
        title: tracked.map(|t| t.meta.title.clone()).unwrap_or_default(),
        from: tracked.and_then(|t| t.meta.from.clone()),
        site: site.map(String::from),
        state: match (site, closed) {
            (Some(_), _) => "unreachable".into(),
            (None, Some(_)) => "closed".into(),
            (None, None) => "removed".into(),
        },
        closed: closed.and_then(|t| t.meta.closed.clone()),
        updated: tracked.map_or(0, |t| t.updated),
        readme: tracked.map(|t| t.readme.clone()).unwrap_or_default(),
        agents: vec![],
    }
}

fn placed_driver_word(config: &Config, site: &str) -> String {
    match config.site(Some(site)) {
        Ok((_, s)) => match s.driver {
            crate::config::DriverKind::Bento => "bento".into(),
            crate::config::DriverKind::Host => "host".into(),
        },
        Err(_) => "unknown".into(),
    }
}

fn launch_reply(l: driver::Launch) -> Reply {
    Reply::Launch {
        argv: l.argv,
        cwd: l.cwd.map(|c| c.to_string_lossy().into_owned()),
        env: l.env,
        path: l.path,
    }
}

/// Connects a stew to the runtime; a failure is a warning (`stew start` connects again).
fn connect(rt: &dyn Runtime, driver: &dyn Driver, id: &str, warnings: &mut Vec<String>) {
    progress(&format!("{id}: connecting it to the agent runtime (hc)"));
    let res = driver
        .root(&Target {
            id: id.into(),
            env: vec![],
        })
        .and_then(|root| rt.connect(id, &runtime::via(id), &root));
    if let Err(e) = res {
        warnings.push(format!(
            "{id} is not connected to the agent runtime: {e} (`stew start {id}` tries again)"
        ));
    }
}

/// Before a stew goes: forget its machine, then stop its own core where it lives (on the
/// host it would outlive the dir, its agents with it). In that order: a machine still
/// there would start the core again through its via. A machine that can't be forgotten is
/// a warning; a core that can't be stopped is the error (its agents may still write).
fn disconnect(
    rt: &dyn Runtime,
    driver: &dyn Driver,
    t: &Target,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if let Err(e) = rt.disconnect(&t.id) {
        warnings.push(format!("disconnecting {} from the runtime: {e}", t.id));
    }
    if let Some(argv) = rt.stop_there() {
        match driver.exec(t, &argv, None, None, true, Some(STOP_TIMEOUT)) {
            Ok(out) if out.ok() => {}
            // stopped or gone, or no hc there (127): nothing of it runs
            Ok(out) if !matches!(out.status, driver::Status::Exited(_)) => {}
            Ok(out) if out.status == driver::Status::Exited(127) => {}
            Ok(out) => {
                return Err(fail!(
                    "stopping {}'s runtime core failed: {}",
                    t.id,
                    out.reason()
                ));
            }
            Err(e) => return Err(fail!("stopping {}'s runtime core failed: {e}", t.id)),
        }
    }
    Ok(())
}

/// The pushed check for `remove`: the look, or None when `force` skipped what couldn't be
/// verified. A finding or a check that can't run refuses (unless `force`: then it is a
/// warning of what is lost). `when` goes into the refusal.
fn verify(
    driver: &dyn Driver,
    t: &Target,
    force: bool,
    when: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<inspect::Look>> {
    let id = &t.id;
    let look = match inspect::look(driver, t, Mode::Check, false, Some(CHECK_TIMEOUT)) {
        Ok(Looked::Look(l)) => l,
        Ok(Looked::NotRunning) if force => {
            warnings.push(format!("not verified (--force): {id} did not start"));
            return Ok(None);
        }
        Ok(Looked::NotRunning) => {
            return Err(Error::Refused(format!(
                "cannot verify that everything in {id} is pushed{when}, nothing removed: it did not start"
            )));
        }
        Err(e) if force => {
            warnings.push(format!("not verified (--force): {e}"));
            return Ok(None);
        }
        Err(e) => {
            return Err(Error::Refused(format!(
                "cannot verify that everything in {id} is pushed{when}, nothing removed: {e}"
            )));
        }
    };
    for w in &look.warnings {
        if !warnings.contains(w) {
            warnings.push(w.clone());
        }
    }
    if !look.findings.is_empty() {
        let list = look.findings.join("\n  ");
        if !force {
            return Err(Error::Refused(format!(
                "not everything in {id} is pushed{when}, nothing removed (--force loses it):\n  {list}"
            )));
        }
        let lost = format!("lost (--force):\n  {list}");
        if !warnings.contains(&lost) {
            warnings.push(lost);
        }
    }
    Ok(Some(look))
}

fn records(resolved: &[Resolved]) -> Vec<RepoRecord> {
    resolved
        .iter()
        .map(|r| RepoRecord {
            name: r.name.clone(),
            remote: r.remote.clone(),
            branch: r.branch.clone(),
        })
        .collect()
}

/// stew's own commit in `notes/`: `stew.yaml` (and a LOG.md line), pushed.
fn commit(
    driver: &dyn Driver,
    t: &Target,
    meta: &StewMeta,
    message: &str,
    line: &str,
) -> Result<()> {
    let yaml = meta_yaml(meta);
    let argv = inspect::sh(inspect::COMMIT, &[message, line]);
    let out = driver.exec(t, &argv, Some(yaml.as_bytes()), None, false, None)?;
    if out.ok() {
        Ok(())
    } else {
        Err(fail!("committing notes/stew.yaml failed: {}", out.reason()))
    }
}
