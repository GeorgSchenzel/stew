# stew's clones, run in a new stew's root through the driver's exec:
#   sh -c "$script" stew <artifacts> <id> <feature branch or ""> [<dir> <remote> <base>]...
# notes/ is a single-branch clone of stew/<id>; each repo is cloned on its base, and with a
# feature branch that branch is created from the base and checked out.
set -eu
# git takes these from the environment over the repo it is in
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES
export GIT_TERMINAL_PROMPT=0
g() { git -c core.hooksPath=/dev/null -c core.fsmonitor=false "$@"; }
artifacts=$1
id=$2
feature=$3
shift 3
g clone -q --single-branch --branch "stew/$id" -- "$artifacts" notes
while [ $# -ge 3 ]; do
	g clone -q --branch "$3" -- "$2" "$1"
	if [ -n "$feature" ]; then
		g -C "$1" switch -q -c "$feature"
	fi
	shift 3
done
