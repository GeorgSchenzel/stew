# Sessions export, run in a stew's root through the driver's exec (stew's code, the same for
# every driver), with each agent's own tools:
#   sh -c "$script" stew list          what there is, then DONE
#   sh -c "$script" stew send <wanted  the wanted sessions' contents, then DONE
# list prints
#   R <root>                                     the root (physical path)
#   C <file> <sha256> <cwd>                      a claude session jsonl whose first cwd is
#                                                the root or below it
#   V opencode <version>
#   O <dir> <n> + n bytes + newline              `opencode session list --format json` run
#                                                in the root (.) and in each clone (<dir>)
#   E <message>                                  something that could not be listed
# send reads `C <file>` and `O <dir> <session>` lines and prints for each
#   F C <file> <n> + n bytes + newline           the file as it is now
#   F O <session> <n> + n bytes + newline        `opencode session export <session>`
#   E <message>
# Fields are tab-separated. Nothing is written in the root: temporary files go to mktemp.
set -u
export GIT_TERMINAL_PROMPT=0
tab=$(printf '\t')
nl='
'
root=$(pwd -P)
here=$(pwd)
tmp=$(mktemp -d) || exit 1
trap 'rm -rf "$tmp"' EXIT

# a record: the header fields, the byte count, the bytes, a newline
emit() {
	n=$(wc -c <"$2" | tr -d ' ')
	printf '%s\t%s\n' "$1" "$n"
	cat "$2"
	printf '\n'
}

err() {
	printf 'E\t%s\n' "$(printf '%s' "$*" | tr '\t\n' '  ')"
}

ours() {
	case $1 in
	"$root" | "$root"/* | "$here" | "$here"/*) return 0 ;;
	esac
	return 1
}

# opencode in its own private server (--standalone): no background service is left behind
oc() {
	opencode "$@" --standalone
}

case ${1:-} in
list)
	printf 'R\t%s\n' "$root"
	# claude: ~/.claude/projects/<the dir, every non-alphanumeric a dash>/<session>.jsonl
	cdir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects
	e1=$(printf '%s' "$root" | sed 's/[^A-Za-z0-9]/-/g')
	e2=$(printf '%s' "$here" | sed 's/[^A-Za-z0-9]/-/g')
	if [ -d "$cdir" ]; then
		for d in "$cdir"/*; do
			[ -d "$d" ] && [ ! -L "$d" ] || continue
			case ${d##*/} in
			"$e1" | "$e1"-* | "$e2" | "$e2"-*) ;;
			*) continue ;;
			esac
			for f in "$d"/*.jsonl; do
				[ -f "$f" ] && [ ! -L "$f" ] || continue
				case $f in *"$tab"* | *"$nl"*) continue ;; esac
				# the encoding is lossy (dir-a and dir/a look the same): the cwd decides
				cwd=$(grep -m1 -o '"cwd":"[^"]*"' "$f" | sed 's/^"cwd":"//; s/"$//')
				ours "$cwd" || continue
				sum=$(sha256sum <"$f") || {
					err "cannot read $f"
					continue
				}
				printf 'C\t%s\t%s\t%s\n' "$f" "${sum%% *}" "$cwd"
			done
		done
	fi
	if command -v opencode >/dev/null 2>&1; then
		v=$(opencode --version 2>/dev/null | head -n 1)
		printf 'V\topencode\t%s\n' "$v"
		for d in . */; do
			d=${d%/}
			if [ "$d" != . ]; then
				[ -d "$d" ] && [ ! -L "$d" ] && [ -e "$d/.git" ] || continue
				case $d in *"$tab"* | *"$nl"*) continue ;; esac
			fi
			if (cd "$d" && oc session list --format json -n 1000000) >"$tmp/l" 2>"$tmp/e"; then
				emit "O$tab$d" "$tmp/l"
			else
				err "opencode session list in $d failed: $(tail -n 3 "$tmp/e")"
			fi
		done
	fi
	;;
send)
	while IFS=$tab read -r kind a b; do
		case $kind in
		C)
			# a copy first: a session written to meanwhile can't change the count
			if [ -f "$a" ] && [ ! -L "$a" ] && cp "$a" "$tmp/x" 2>"$tmp/e"; then
				emit "F${tab}C$tab$a" "$tmp/x"
			else
				err "cannot read $a"
			fi
			;;
		O)
			if (cd "$a" && oc session export "$b") >"$tmp/x" 2>"$tmp/e"; then
				emit "F${tab}O$tab$b" "$tmp/x"
			else
				err "opencode session export $b in $a failed: $(tail -n 3 "$tmp/e")"
			fi
			;;
		*) err "bad request line: $kind" ;;
		esac
	done
	;;
*)
	err "usage: list | send"
	exit 2
	;;
esac
printf 'DONE\n'
