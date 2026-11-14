---
name: stew
description: Stews are workspaces of repo clones in a sandbox. Use to list or start stews,
  hand work to a new stew, read another stew's notes, or add a repo.
---
`stew` here is this system's CLI, not the binary installer of the same name.
You are in a stew: a dir of writable repo clones plus `notes/`, usually in a sandbox
(box) of its own; `stew info` says which.

- `notes/` is this stew's own record (plans, findings, anything worth keeping). It is a
  git checkout: commit and push it like code. Its `stew.yaml` belongs to stew; leave it.
- Repos: plain git. Branch, commit, push as the work needs; pushing is how work leaves the box.
- `stew info`: this stew. `stew repos`: repos you can clone in (`git clone <url>`;
  its declared ports are published at the next `stew start`, which a human does).
- `stew new <repos…> --title <t> [--branch[=name]] [--base b] [--brief f [--start]]
  [--agent a] [--model m]`: a new stew started from this one; prints its id. With
  `--brief`, the file becomes its `notes/BRIEF.md`; with `--start`, an agent begins on it.
  Without `--start` it waits for a human.
- Agents in this stew: hc, see `hc --skill`. Agents in a new stew: `stew new`.
- `stew agent <id> "<task>"`: an agent in another stew you started (e.g. a prepared one).
- `stew ls --from .`: stews started from here, with their agents' status and Now lines.
- Another stew's notes: `git fetch origin stew/<id>:refs/remotes/origin/stew/<id>`,
  then `git show origin/stew/<id>:<path>`. Links look like `<id>:<path>`, `<repo>@<branch>`.
- `notes/README.md`: a short paragraph telling someone new what this stew is and why it
  exists, then `## Now`, where things stand. Keep it true; start a Now line with
  `waiting:` or `blocked:` when it needs a human.
- `notes/LOG.md`: append `- <date> <what happened>` when something worth remembering
  happens; high-level, not per commit.
