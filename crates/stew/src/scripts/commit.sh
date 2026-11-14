# stew's own commit in notes/, run in the stew's root through the driver's exec:
#   sh -c "$script" stew <message> <LOG.md line or ""> < stew.yaml
# Writes stew.yaml (stdin), appends the line to LOG.md, commits only those files, then
# pull --rebase --autostash and push, so the agent's uncommitted edits neither block it nor
# get committed.
set -eu
# git takes these from the environment over the repo it is in
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES
export GIT_TERMINAL_PROMPT=0
g() { git -c core.hooksPath=/dev/null -c core.fsmonitor=false "$@"; }
cd notes
message=$1
line=$2
cat >.stew.yaml.tmp
mv .stew.yaml.tmp stew.yaml
set -- stew.yaml
if [ -n "$line" ]; then
	printf '%s\n' "$line" >>LOG.md
	set -- stew.yaml LOG.md
fi
if ! g config user.email >/dev/null || ! g config user.name >/dev/null; then
	export GIT_AUTHOR_NAME=stew GIT_AUTHOR_EMAIL=stew@localhost
	export GIT_COMMITTER_NAME=stew GIT_COMMITTER_EMAIL=stew@localhost
fi
# `add` first: a LOG.md the agent deleted is new again
g add -- "$@"
if ! g diff --cached --quiet HEAD -- "$@"; then
	g commit -q -m "$message" -- "$@"
fi
g pull -q --rebase --autostash
g push -q
