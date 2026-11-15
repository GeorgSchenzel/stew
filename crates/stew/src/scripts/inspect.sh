# stew's look at a stew root (the cwd), run where the stew lives through the driver's exec:
#   sh -c "$script" stew <mode> <local-remotes>
# mode: `ls` (what is there) or `check` (also: is everything pushed?).
# local-remotes: `outside` (a remote that is a local path outside the root survives a
# remove: the host driver) or `none` (no local path survives: a box).
#
# Output, one item per line:
#   R <dir>\t<remote url>\t<branch>\t<its default>   a clone (the snapshot); its remote is
#                                                   origin, else its only one
#   Y <line>   notes/stew.yaml        N <line>   notes/README.md
#   F <where>: <what>   something a remove would lose (check)
#   W <message>
#   DONE       the last line; without it the look failed
# Fail-closed: anything this can't verify is an F line. The check changes no ref, no
# index and no file of the repos it looks at (its fetch writes only into its own
# namespace, removed again).

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
	# never a prompt, never a fetch that hangs: a stalled connection fails (and then counts
	# for nothing: fail-closed)
	export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh} -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
	export GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=60
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

# Whether remote url $1 of the repo at $2 survives a remove (0) or not (1). A network URL
# survives, except one that points back at the stew's own machine when nothing local
# survives (a box: a git server in it goes with it).
survives() {
	url=$1
	host=""
	case $url in
	file://*) p=${url#file://} ;;
	*://*)
		host=${url#*://}
		host=${host%%/*}
		host=${host##*@}
		;;
	*::*) return 0 ;; # <transport>::<address>
	/* | ./* | ../* | . | ..) p=$url ;;
	*:*)
		case ${url%%:*} in
		*/*) p=$url ;;
		*)
			host=${url%%:*} # scp-like host:path
			host=${host##*@}
			;;
		esac
		;;
	*) p=$url ;;
	esac
	if [ -n "$host" ]; then
		[ "$local_remotes" = none ] || return 0
		case $host in
		"["*)
			host=${host#?}
			host=${host%%"]"*}
			;;
		*:*:*) ;; # a bare IPv6 address
		*) host=${host%%:*} ;;
		esac
		case $host in
		localhost | localhost.* | 127.* | ::1 | 0.0.0.0 | 0:0:0:0:0:0:0:1) return 1 ;;
		esac
		return 0
	fi
	[ "$local_remotes" = outside ] || return 1
	case $p in /*) ;; *) p=$2/$p ;; esac
	# git also tries <path>.git
	rp=$(cd "$p" 2>/dev/null && pwd -P) || rp=$(cd "$p.git" 2>/dev/null && pwd -P) || return 1
	case $rp/ in "$root"/*) return 1 ;; esac
	return 0
}

# Where the check fetches the remotes' heads and tags: a namespace of its own, removed
# again. The fetch never writes any other ref: explicit refspecs (the repo's own fetch
# refspecs, mirror or forced ones, are not used), no tags, no prune, no FETCH_HEAD, no
# opportunistic tracking-ref updates, no submodules, no gc.
NS=refs/stew-pushed-check

drop_ns() {
	G for-each-ref --format='delete %(refname)' "$NS/" | G update-ref --stdin
}

# Are all refs of the repo `G` reaches, and the detached HEADs in $detached, on a remote
# that survives? F and W lines for $label; relative remote URLs start at $base.
check_refs() {
	# the refs as they are, before anything is fetched
	refs=$(G for-each-ref --format='%(objectname) %(refname)' 2>&1) || {
		findings "$label: cannot list refs" "$refs"
		return 0
	}
	drop_ns 2>/dev/null
	nots=""
	rtags=""
	counted=0
	i=0
	for rem in $(G remote); do
		i=$((i + 1))
		url=$(G ls-remote --get-url "$rem" 2>/dev/null) || url=""
		if ! survives "$url" "$base"; then
			printf 'W %s: remote %s (%s) is removed with the stew; it does not count\n' "$label" "$rem" "$url"
			continue
		fi
		if ! out=$(G -c fetch.prune=false -c fetch.pruneTags=false -c "remote.$rem.prune=false" \
			-c "remote.$rem.pruneTags=false" -c gc.auto=0 -c maintenance.auto=false \
			-c fetch.writeCommitGraph=false -c submodule.recurse=false \
			fetch -q --no-tags --no-write-fetch-head --refmap= --no-recurse-submodules "$rem" \
			"+refs/heads/*:$NS/$i/heads/*" "+refs/tags/*:$NS/$i/tags/*" 2>&1); then
			printf 'W %s: cannot fetch %s: %s\n' "$label" "$rem" "${out%%"$nl"*}"
			continue
		fi
		if ! out=$(G ls-remote "$rem" 2>&1); then
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
	{
		printf '%s\n' "$refs"
		printf '%s' "$detached" | while IFS="$tab" read -r sha where; do
			printf '%s DETACHED %s\n' "$sha" "$where"
		done
	} | while read -r sha ref rest; do
		[ -n "$sha" ] || continue
		case $ref in
		refs/remotes/* | refs/prefetch/* | refs/stash | "$NS"/*) continue ;;
		refs/tags/*)
			case $nl$rtags in
			*"$nl$sha $ref$nl"*) ;;
			*) printf 'F %s: tag %s is on no remote\n' "$label" "${ref#refs/tags/}" ;;
			esac
			continue
			;;
		esac
		n=$(printf '%s' "$nots" | G rev-list --count --ignore-missing --stdin "$sha" 2>&1) || {
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
	drop_ns 2>/dev/null
	return 0
}

# A worktree that exists: its files, an operation in progress (in its own git dir), and
# repos inside it that it ignores (their commits are not ignored files).
check_tree() {
	w=$1
	where=$2
	wgd=$(g -C "$w" rev-parse --absolute-git-dir 2>&1) || {
		findings "$where: no git dir" "$wgd"
		return 0
	}
	for p in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG; do
		[ -e "$wgd/$p" ] && printf 'F %s: an operation is in progress (%s)\n' "$where" "$p"
	done
	st=$(g -C "$w" status --porcelain=v1 --untracked-files=all --ignore-submodules=none 2>&1) || {
		findings "$where: git status failed" "$st"
		return 0
	}
	[ -n "$st" ] && findings "$where: uncommitted changes or untracked files" "$st"
	files=$(g -C "$w" ls-files -v 2>&1) || {
		findings "$where: git ls-files failed" "$files"
		return 0
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
	ign=$(g -C "$w" ls-files --others --ignored --exclude-standard --directory 2>&1) || {
		findings "$where: cannot list ignored files" "$ign"
		return 0
	}
	printf '%s\n' "$ign" | while IFS= read -r e; do
		case $e in */) ;; *) continue ;; esac
		find "$w/$e" -name .git 2>/dev/null | while IFS= read -r dotgit; do
			nested=${dotgit%/.git}
			check_repo "$nested" "$where/${nested#"$w"/}"
		done
	done
	return 0
}

# A submodule's git dir that nothing checks out any more (`submodule deinit`, or the
# checkout removed): its refs and HEAD.
check_module() (
	d=$1
	label=$2
	base=$d
	G() { g --git-dir="$d" -c core.bare=true "$@"; }
	detached=""
	if ! G symbolic-ref -q HEAD >/dev/null; then
		h=$(G rev-parse -q --verify HEAD) && detached="$h$tab$label$nl"
	fi
	check_refs
)

# Checks the repo at $1 (shown as $2), its worktrees and submodules. A subshell, so its
# variables (and G) stay its own.
check_repo() (
	r=$1
	label=$2
	top=$(g -C "$r" rev-parse --show-toplevel 2>&1) || {
		findings "$label: not a readable git repo" "$top"
		exit 0
	}
	base=$top
	G() { g -C "$top" "$@"; }
	if G rev-parse -q --verify refs/stash >/dev/null; then
		printf 'F %s: has a stash\n' "$label"
	fi

	# every worktree: files, operations, a detached HEAD; one whose dir is gone still has
	# its HEAD in the repo
	wts=$(G worktree list --porcelain 2>&1) || {
		findings "$label: cannot list worktrees" "$wts"
		exit 0
	}
	detached=""
	first=1
	w=""
	head=""
	det=0
	while IFS= read -r line; do
		case $line in
		"worktree "*)
			w=${line#worktree }
			# the first is the main worktree (a submodule's shows as its git dir)
			if [ $first = 1 ]; then
				w=$top
				first=0
			fi
			head=""
			det=0
			;;
		"HEAD "*) head=${line#HEAD } ;;
		detached) det=1 ;;
		"")
			[ -n "$w" ] || continue
			where=$label
			[ "$w" = "$top" ] || where="$label (worktree $w)"
			if [ -d "$w" ]; then
				check_tree "$w" "$where"
				if ! g -C "$w" symbolic-ref -q HEAD >/dev/null; then
					h=$(g -C "$w" rev-parse -q --verify HEAD) && detached="$detached$h$tab$where$nl"
				fi
			elif [ $det = 1 ] && [ -n "$head" ]; then
				detached="$detached$head$tab$where (its dir is gone)$nl"
			fi
			w=""
			;;
		esac
	done <<EOF
$wts

EOF

	check_refs

	# initialized submodules, each checked like a repo of its own
	subs=$(G submodule foreach --quiet 'printf "%s\n" "$sm_path"' 2>&1) || {
		findings "$label: cannot list submodules" "$subs"
		exit 0
	}
	seen=""
	while IFS= read -r s; do
		[ -n "$s" ] || continue
		if [ -e "$top/$s/.git" ]; then
			check_repo "$top/$s" "$label/$s"
			sgd=$(g -C "$top/$s" rev-parse --absolute-git-dir 2>/dev/null) && seen="$seen$sgd$nl"
		fi
	done <<EOF
$subs
EOF
	# submodule git dirs nobody checks out any more
	common=$(cd "$top" && cd "$(G rev-parse --git-common-dir)" && pwd -P) || {
		findings "$label: no common git dir" "$common"
		exit 0
	}
	[ -d "$common/modules" ] || exit 0
	find "$common/modules" -name HEAD -type f | while IFS= read -r headfile; do
		d=${headfile%/HEAD}
		[ -d "$d/objects" ] && [ -d "$d/refs" ] || continue
		skip=0
		while IFS= read -r v; do
			[ -n "$v" ] || continue
			case $d/ in "$v"/*) skip=1 ;; esac
		done <<EOF
$seen
EOF
		[ $skip = 1 ] && continue
		check_module "$d" "$label (module ${d#"$common/modules/"})"
	done
)

for d in * .[!.]* ..?*; do
	[ -e "$d" ] || [ -L "$d" ] || continue # an unmatched glob
	[ -L "$d" ] && continue                # a symlink: removing it loses nothing of its own
	if [ "$d" = notes ]; then
		continue
	fi
	if [ -d "$d" ] && [ -e "$d/.git" ]; then
		# its remote: origin, else the only one it has
		rem=origin
		url=$(g -C "$d" remote get-url origin 2>/dev/null) || url=""
		if [ -z "$url" ]; then
			rems=$(g -C "$d" remote 2>/dev/null) || rems=""
			case $rems in
			*"$nl"* | "") ;;
			*)
				rem=$rems
				url=$(g -C "$d" remote get-url "$rem" 2>/dev/null) || url=""
				;;
			esac
		fi
		br=$(g -C "$d" symbolic-ref -q --short HEAD 2>/dev/null) || br=""
		def=$(g -C "$d" symbolic-ref -q --short "refs/remotes/$rem/HEAD" 2>/dev/null) || def=""
		printf 'R %s\t%s\t%s\t%s\n' "$d" "$url" "$br" "${def#"$rem"/}"
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
