//! The hub's commands on stews: new, open, remove, close, start, stop, title, ls, and where
//! `exec`/`enter` run. The daemon reaches a stew only through its site's driver; all git in
//! clones and `notes/` runs where the stew lives, through `exec` (`inspect`).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use stew_proto::{
    AgentInfo, CreationInfo, Here, Live, LiveRepo, Openness, Reply, RepoRecord, Resolved, StewMeta,
    Tracked,
};

use crate::catalog::Catalog;
use crate::config::Config;
use crate::daemon::Hub;
use crate::driver::{self, BoxState, Driver, ExecOpts, Target};
use crate::error::{Error, Result, fail};
use crate::git::git;
use crate::inspect::{self, Look, Looked, Mode};
use crate::mirror::{meta_yaml, parse_meta};
use crate::runtime::{self, Runtime};
use crate::state::{Creation, Placed, Progress};
use crate::{agent, id, readme, refs};

/// How long `ls` waits for one stew before showing it unreachable.
const LS_TIMEOUT: Duration = Duration::from_secs(20);

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
        let hub = self.socket.to_string_lossy();
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
        if (a.agent.is_some() || a.model.is_some()) && !a.start {
            return Err(fail!("--agent and --model go with --start"));
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
            let (agent, model) = agent::choose(&config, a.agent.as_deref(), a.model.as_deref());
            Some((
                agent.clone(),
                agent::command(&agent, model.as_deref(), agent::BRIEF_TASK)?,
            ))
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
        )?;
        let agent = match launch {
            None => None,
            Some((name, argv)) => {
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
        let reopen = meta.is_closed().then(|| StewMeta {
            state: Some(Openness::Open),
            closed: None,
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
        )?;
        Ok(Reply::Created {
            id: id.to_string(),
            agent: None,
            warnings,
        })
    }

    /// The driver's create, clones through `exec`, (reopen: the commit), the driver's
    /// start, the runtime's connect. A failure up to the start removes the stew from the
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
            let out = driver.exec(&t, &inspect::sh(inspect::CLONE, &args), None, false, None)?;
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
            driver.start(&t)
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
                        },
                    );
                })?;
                connect(rt, id, warnings);
                Ok(())
            }
            Err(e) => {
                let mut msg = e.to_string();
                if let Err(e2) = driver.remove(&t) {
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

        let look = match inspect::look(driver.as_ref(), &t, Mode::Check, false, None) {
            Ok(Looked::Look(l)) => Some(l),
            Ok(Looked::NotRunning) if force => {
                warnings.push(format!("not verified (--force): {id} did not start"));
                None
            }
            Ok(Looked::NotRunning) => {
                return Err(Error::Refused(format!(
                    "cannot verify that everything in {id} is pushed, nothing removed: it did not start"
                )));
            }
            Err(e) if force => {
                warnings.push(format!("not verified (--force): {e}"));
                None
            }
            Err(e) => {
                return Err(Error::Refused(format!(
                    "cannot verify that everything in {id} is pushed, nothing removed: {e}"
                )));
            }
        };
        if let Some(l) = &look {
            warnings.extend(l.warnings.iter().cloned());
            if !l.findings.is_empty() {
                let list = l.findings.join("\n  ");
                if !force {
                    return Err(Error::Refused(format!(
                        "not everything in {id} is pushed, nothing removed (--force loses it):\n  {list}"
                    )));
                }
                warnings.push(format!("lost (--force):\n  {list}"));
            }
        }

        // the snapshot (and for close, the state and the LOG line), committed and pushed
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
                        return Err(fail!("writing the snapshot failed, nothing removed: {e}"));
                    }
                }
            }
            None if close => warnings.push(format!("{id} is not marked closed: no snapshot")),
            None => {}
        }
        // B3: export the stew's sessions to the transcripts repo here
        disconnect(rt.as_ref(), driver.as_ref(), &t, &mut warnings);
        driver.remove(&t)?;
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
                let mut w = driver.start(&target(i, &placed.token))?;
                connect(runtime::for_config(&config).as_ref(), i, &mut w);
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
        match inspect::look(driver.as_ref(), &t, Mode::Check, true, None) {
            Ok(Looked::NotRunning) => {}
            Ok(Looked::Look(l)) => {
                warnings.extend(l.warnings.iter().cloned());
                if !l.findings.is_empty() {
                    warnings.push(format!(
                        "not everything in {id} is pushed:\n  {}",
                        l.findings.join("\n  ")
                    ));
                }
                // B3: export its sessions here, before it stops
                if let Err(e) =
                    self.snapshot(&config, driver.as_ref(), &t, &l, false, &mut warnings)
                {
                    warnings.push(format!("no snapshot: {e}"));
                }
            }
            Err(e) => warnings.push(format!("no snapshot: {e}")),
        }
        warnings.extend(driver.stop(&t)?);
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
                },
            )?
        };
        Ok(launch_reply(l))
    }

    pub fn ls(&self) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, creations) = {
            let f = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (f.state.stews.clone(), f.state.creations.clone())
        };
        let mut warnings = Vec::new();
        let agents = self.agents(&config, &mut warnings);
        let tracked: BTreeMap<String, Tracked> = match self.mirror_every(&config, &mut warnings) {
            Ok(all) => all.into_iter().map(|t| (t.meta.id.clone(), t)).collect(),
            Err(e) => {
                warnings.push(e.to_string());
                BTreeMap::new()
            }
        };
        let mut stews: Vec<Live> = std::thread::scope(|s| {
            let jobs: Vec<_> = placed
                .iter()
                .map(|(id, p)| {
                    let config = &config;
                    let tracked = tracked.get(id);
                    let agents = agents.get(id).cloned().unwrap_or_default();
                    s.spawn(move || self.live(config, id, p, tracked, agents))
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().ok()).collect()
        });
        stews.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.id.cmp(&b.id)));
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
        Ok(Reply::Live {
            stews,
            creations,
            warnings,
        })
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

    /// One stew for `ls`: live when it runs (never starting it), else its notes branch.
    fn live(
        &self,
        config: &Config,
        id: &str,
        p: &Placed,
        tracked: Option<&Tracked>,
        agents: Vec<AgentInfo>,
    ) -> Live {
        let mut live = from_tracked(id, tracked, &p.site, "unreachable");
        live.agents = agents;
        let Ok((_, driver)) = self.driver(config, Some(&p.site)) else {
            return live;
        };
        let t = target(id, &p.token);
        let state = match driver.state(&t) {
            Ok(s) => s,
            Err(_) => return live,
        };
        live.state = state.word().to_string();
        if state != BoxState::Running {
            return live;
        }
        match inspect::look(driver.as_ref(), &t, Mode::Ls, true, Some(LS_TIMEOUT)) {
            Ok(Looked::Look(l)) => {
                live.repos = l
                    .repos
                    .iter()
                    .map(|f| LiveRepo {
                        dir: f.dir.clone(),
                        branch: f.branch.clone(),
                    })
                    .collect();
                if let Some(m) = l.meta.as_deref().and_then(|m| parse_meta(m).ok()) {
                    live.title = m.title;
                    live.from = m.from;
                }
                if let Some(r) = &l.readme {
                    live.now = readme::summarize(r).now;
                }
            }
            Ok(Looked::NotRunning) => live.state = "stopped".into(),
            Err(_) => live.state = "unreachable".into(),
        }
        live
    }
}

impl Hub {
    /// The caller's own stew, live.
    pub fn info(&self, id: &str) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (placed, driver) = self.placed(&config, id)?;
        let mut warnings = Vec::new();
        let agents = self.agents(&config, &mut warnings);
        let tracked = self
            .mirror_every(&config, &mut warnings)
            .ok()
            .and_then(|all| all.into_iter().find(|t| t.meta.id == id));
        let stew = self.live(
            &config,
            id,
            &placed,
            tracked.as_ref(),
            agents.get(id).cloned().unwrap_or_default(),
        );
        let root = driver.root(&target(id, &placed.token))?;
        Ok(Reply::Info {
            stew,
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
        task: &str,
        caller: Option<&str>,
    ) -> Result<Reply> {
        let config = Config::load(&self.paths)?;
        let (agent, model) = agent::choose(&config, agent, model);
        let argv = agent::command(&agent, model.as_deref(), task)?;
        let (placed, driver) = self.placed(&config, id)?;
        let mut warnings = Vec::new();
        if let Some(c) = caller.filter(|c| *c != id) {
            let from = self
                .mirror_every(&config, &mut warnings)?
                .into_iter()
                .find(|t| t.meta.id == id)
                .and_then(|t| t.meta.from);
            if from.as_deref() != Some(c) {
                return Err(Error::Refused(format!(
                    "{id} was not started from {c}: agents go only into stews you started"
                )));
            }
        }
        let _busy = self.busy(id)?;
        let t = target(id, &placed.token);
        warnings.extend(driver.start(&t)?);
        let rt = runtime::for_config(&config);
        rt.connect(id, &runtime::via(id))?;
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

    /// The tracked stews started from `id`, open and closed, live where they exist.
    pub fn ls_from(&self, id: &str) -> Result<Reply> {
        id::check(id)?;
        let config = Config::load(&self.paths)?;
        let mut warnings = Vec::new();
        let agents = self.agents(&config, &mut warnings);
        let placed = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .stews
            .clone();
        let mut children: Vec<Tracked> = self
            .mirror_every(&config, &mut warnings)?
            .into_iter()
            .filter(|t| t.meta.from.as_deref() == Some(id))
            .collect();
        children.sort_by_key(|t| std::cmp::Reverse(t.updated));
        let stews = std::thread::scope(|s| {
            let jobs: Vec<_> = children
                .iter()
                .map(|t| {
                    let config = &config;
                    let p = placed.get(&t.meta.id);
                    let agents = agents.get(&t.meta.id).cloned().unwrap_or_default();
                    s.spawn(move || match p {
                        Some(p) => self.live(config, &t.meta.id, p, Some(t), agents),
                        None => {
                            let word = if t.meta.is_closed() {
                                "closed"
                            } else {
                                "removed"
                            };
                            from_tracked(&t.meta.id, Some(t), "", word)
                        }
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().ok()).collect()
        });
        Ok(Reply::Live {
            stews,
            creations: vec![],
            warnings,
        })
    }
}

/// A stew as its notes branch has it (the state a word for where it is).
fn from_tracked(id: &str, tracked: Option<&Tracked>, site: &str, state: &str) -> Live {
    Live {
        id: id.to_string(),
        title: tracked.map(|t| t.meta.title.clone()).unwrap_or_default(),
        from: tracked.and_then(|t| t.meta.from.clone()),
        site: site.to_string(),
        state: state.into(),
        repos: tracked
            .map(|t| {
                t.meta
                    .repos
                    .iter()
                    .map(|r| LiveRepo {
                        dir: r.name.rsplit('/').next().unwrap_or(&r.name).to_string(),
                        branch: r.branch.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        now: tracked.map(|t| t.now.clone()).unwrap_or_default(),
        updated: tracked.map_or(0, |t| t.updated),
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
fn connect(rt: &dyn Runtime, id: &str, warnings: &mut Vec<String>) {
    if let Err(e) = rt.connect(id, &runtime::via(id)) {
        warnings.push(format!(
            "{id} is not connected to the agent runtime: {e} (`stew start {id}` tries again)"
        ));
    }
}

/// Before a stew goes: forget its machine, then stop its own core where it lives (on the
/// host it would outlive the dir, its agents with it). In that order: a machine still
/// there would start the core again through its via. Failures are warnings.
fn disconnect(rt: &dyn Runtime, driver: &dyn Driver, t: &Target, warnings: &mut Vec<String>) {
    if let Err(e) = rt.disconnect(&t.id) {
        warnings.push(format!("disconnecting {} from the runtime: {e}", t.id));
    }
    if let Some(argv) = rt.stop_there() {
        match driver.exec(t, &argv, None, true, Some(STOP_TIMEOUT)) {
            Ok(out) if out.ok() => {}
            // stopped or gone: nothing of it runs
            Ok(out) if !matches!(out.status, driver::Status::Exited(_)) => {}
            Ok(out) => warnings.push(format!(
                "stopping {}'s runtime core: {}",
                t.id,
                out.reason()
            )),
            Err(e) => warnings.push(format!("stopping {}'s runtime core: {e}", t.id)),
        }
    }
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
    let out = driver.exec(t, &argv, Some(yaml.as_bytes()), false, None)?;
    if out.ok() {
        Ok(())
    } else {
        Err(fail!("committing notes/stew.yaml failed: {}", out.reason()))
    }
}
