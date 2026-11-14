# stew's look at a stew root (the cwd), run where the stew lives through the driver's exec:
#   sh -c "$script" stew <mode> <local-remotes>
# mode: `ls` (what is there) or `check` (also: is everything pushed?).
# local-remotes: `outside` (a remote that is a local path outside the root survives a
# remove: the host driver) or `none` (no local path survives: a box).
#
# Output, one item per line:
#   R <dir>\t<origin url>\t<branch>\t<origin's default>   a clone (the snapshot)
#   Y <line>   notes/stew.yaml        N <line>   notes/README.md
#   F <where>: <what>   something a remove would lose (check)
#   W <message>
#   DONE       the last line; without it the look failed
# Fail-closed: anything this can't verify is an F line.

set -u
mode=$1
local_remotes=$2
root=$(pwd -P) || exit 1
tab=$(printf '\t')
nl='
'
# git takes these from the environment over the repo it is in
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES
export GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0 LC_ALL=C
if [ "$mode" = check ]; then
	export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh} -o BatchMode=yes"
fi

g() { git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.quotePath=true "$@"; }

# file content, each line prefixed
dump() {
	[ -f "$2" ] || return 0
	while IFS= read -r line || [ -n "$line" ]; do
		printf '%s %s\n' "$1" "$line"
	done <"$2"
}

# up to 5 lines of $2 as F lines under the heading $1
findings() {
	printf 'F %s\n' "$1"
	n=0
	printf '%s\n' "$2" | while IFS= read -r line; do
		n=$((n + 1))
		if [ $n -le 5 ]; then
			printf 'F     %s\n' "$line"
		elif [ $n -eq 6 ]; then
			printf 'F     ...\n'
		fi
	done
}

# Whether remote url $1 of the repo at $2 survives a remove (0) or not (1); non-local: 0.
survives() {
	url=$1
	case $url in
	file://*) p=${url#file://} ;;
	*://*) return 0 ;;
	*::*) return 0 ;;
	/* | ./* | ../* | . | ..) p=$url ;;
	*:*)
		case ${url%%:*} in
		*/*) p=$url ;;
		*) return 0 ;; # scp-like host:path
		esac
		;;
	*) p=$url ;;
	esac
	[ "$local_remotes" = outside ] || return 1
	case $p in /*) ;; *) p=$2/$p ;; esac
	# git also tries <path>.git
	rp=$(cd "$p" 2>/dev/null && pwd -P) || rp=$(cd "$p.git" 2>/dev/null && pwd -P) || return 1
	case $rp/ in "$root"/*) return 1 ;; esac
	return 0
}

# Checks the repo at $1 (shown as $2), its worktrees and submodules. A subshell, so its
# variables stay its own.
check_repo() (
	r=$1
	label=$2
	top=$(g -C "$r" rev-parse --show-toplevel 2>&1) || {
		findings "$label: not a readable git repo" "$top"
		exit 0
	}
	gitdir=$(g -C "$top" rev-parse --absolute-git-dir 2>&1) || {
		findings "$label: no git dir" "$gitdir"
		exit 0
	}
	for p in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG; do
		[ -e "$gitdir/$p" ] && printf 'F %s: an operation is in progress (%s)\n' "$label" "$p"
	done
	if g -C "$top" rev-parse -q --verify refs/stash >/dev/null; then
		printf 'F %s: has a stash\n' "$label"
	fi

	# every worktree: uncommitted or untracked files, hidden changes, a detached HEAD
	wts=$(g -C "$top" worktree list --porcelain 2>&1) || {
		findings "$label: cannot list worktrees" "$wts"
		exit 0
	}
	detached=""
	first=1
	while IFS= read -r line; do
		case $line in "worktree "*) ;; *) continue ;; esac
		w=${line#worktree }
		where=$label
		# the first is the main worktree (a submodule's shows as its git dir)
		if [ $first = 1 ]; then
			w=$top
			first=0
		fi
		[ "$w" = "$top" ] || where="$label (worktree $w)"
		[ -d "$w" ] || continue # a pruned worktree's entry: nothing on disk to lose
		st=$(g -C "$w" status --porcelain=v1 --untracked-files=all --ignore-submodules=none 2>&1) || {
			findings "$where: git status failed" "$st"
			continue
		}
		[ -n "$st" ] && findings "$where: uncommitted changes or untracked files" "$st"
		files=$(g -C "$w" ls-files -v 2>&1) || {
			findings "$where: git ls-files failed" "$files"
			continue
		}
		sparse=$(g -C "$w" config --bool core.sparseCheckout 2>/dev/null) || sparse=false
		hidden=""
		while IFS= read -r f; do
			case $f in
			[a-z]" "*) hidden="$hidden$f$nl" ;;
			"S "*) [ "$sparse" = true ] || hidden="$hidden$f$nl" ;;
			esac
		done <<EOF
$files
EOF
		[ -n "$hidden" ] && findings "$where: files marked assume-unchanged or skip-worktree (status can't see their changes)" "$hidden"
		if ! g -C "$w" symbolic-ref -q HEAD >/dev/null; then
			h=$(g -C "$w" rev-parse -q --verify HEAD) && detached="$detached$h$tab$where$nl"
		fi
	done <<EOF
$wts
EOF

	# what the remotes have: fetched, then listed (tracking refs are not trusted)
	nots=""
	rtags=""
	counted=0
	for rem in $(g -C "$top" remote); do
		url=$(g -C "$top" ls-remote --get-url "$rem" 2>/dev/null) || url=""
		if ! survives "$url" "$top"; then
			printf 'W %s: remote %s (%s) is removed with the stew; it does not count\n' "$label" "$rem" "$url"
			continue
		fi
		if ! out=$(g -C "$top" fetch -q "$rem" 2>&1); then
			printf 'W %s: cannot fetch %s: %s\n' "$label" "$rem" "${out%%"$nl"*}"
			continue
		fi
		if ! out=$(g -C "$top" ls-remote "$rem" 2>&1); then
			printf 'W %s: cannot list %s: %s\n' "$label" "$rem" "${out%%"$nl"*}"
			continue
		fi
		counted=$((counted + 1))
		while IFS="$tab" read -r sha ref; do
			[ -n "$sha" ] || continue
			nots="$nots^$sha$nl"
			case $ref in refs/tags/*) rtags="$rtags$sha $ref$nl" ;; esac
		done <<EOF
$out
EOF
	done

	refs=$(g -C "$top" for-each-ref --format='%(objectname) %(refname)' 2>&1) || {
		findings "$label: cannot list refs" "$refs"
		exit 0
	}
	{
		printf '%s\n' "$refs"
		printf '%s' "$detached" | while IFS="$tab" read -r sha where; do
			printf '%s DETACHED %s\n' "$sha" "$where"
		done
	} | while read -r sha ref rest; do
		[ -n "$sha" ] || continue
		case $ref in
		refs/remotes/* | refs/prefetch/* | refs/stash) continue ;;
		refs/tags/*)
			case $nl$rtags in
			*"$nl$sha $ref$nl"*) ;;
			*) printf 'F %s: tag %s is on no remote\n' "$label" "${ref#refs/tags/}" ;;
			esac
			continue
			;;
		esac
		n=$(printf '%s' "$nots" | g -C "$top" rev-list --count --ignore-missing --stdin "$sha" 2>&1) || {
			findings "$label: cannot check $ref" "$n"
			continue
		}
		case $n in
		0) ;;
		*[!0-9]*) findings "$label: cannot check $ref" "$n" ;;
		*)
			if [ "$ref" = DETACHED ]; then
				printf 'F %s: a detached HEAD has %s commit(s) on no remote\n' "$rest" "$n"
			else
				what=$ref
				case $ref in refs/heads/*) what="branch ${ref#refs/heads/}" ;; esac
				if [ $counted -eq 0 ]; then
					printf 'F %s: %s has %s commit(s) and no remote to check against\n' "$label" "$what" "$n"
				else
					printf 'F %s: %s has %s commit(s) on no remote\n' "$label" "$what" "$n"
				fi
			fi
			;;
		esac
	done

	# initialized submodules, each checked like a repo of its own
	subs=$(g -C "$top" submodule foreach --quiet 'printf "%s\n" "$sm_path"' 2>&1) || {
		findings "$label: cannot list submodules" "$subs"
		exit 0
	}
	printf '%s\n' "$subs" | while IFS= read -r s; do
		[ -n "$s" ] || continue
		if [ -e "$top/$s/.git" ]; then
			check_repo "$top/$s" "$label/$s"
		fi
	done
)

for d in * .[!.]* ..?*; do
	[ -e "$d" ] || [ -L "$d" ] || continue # an unmatched glob
	[ -L "$d" ] && continue                # a symlink: removing it loses nothing of its own
	if [ "$d" = notes ]; then
		continue
	fi
	if [ -d "$d" ] && [ -e "$d/.git" ]; then
		url=$(g -C "$d" remote get-url origin 2>/dev/null) || url=""
		br=$(g -C "$d" symbolic-ref -q --short HEAD 2>/dev/null) || br=""
		def=$(g -C "$d" symbolic-ref -q --short refs/remotes/origin/HEAD 2>/dev/null) || def=""
		printf 'R %s\t%s\t%s\t%s\n' "$d" "$url" "$br" "${def#origin/}"
		[ "$mode" = check ] && check_repo "$d" "$d"
	elif [ "$mode" = check ]; then
		# an empty dir (lost+found on a fresh disk) holds nothing to lose
		if [ -d "$d" ] && ls -A "$d" >/dev/null 2>&1 && [ -z "$(ls -A "$d")" ]; then
			continue
		fi
		printf 'F %s: not in a repo (removed with the stew)\n' "$d"
	fi
done

if [ -d notes ] && [ -e notes/.git ]; then
	[ "$mode" = check ] && check_repo notes notes
elif [ "$mode" = check ]; then
	printf 'F notes: missing or not a git repo\n'
fi
dump Y notes/stew.yaml
dump N notes/README.md
printf 'DONE\n'
