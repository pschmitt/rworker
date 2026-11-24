# The SSH session runs this script with the original remote command as $1 and the
# worker's topic, if any, as $2.
set -u

root="$HOME/.local/state/rworker/jobs"
job="$root/exec"
previous_umask=$(umask)
umask 077
mkdir -p -- "$root" || exit 1
staging=$(mktemp -d "$root/.exec.XXXXXX") || exit 1
trap 'rm -rf -- "$staging"' EXIT

date -u +%FT%TZ > "$staging/started" || exit 1
printf '%s\n' "$$" > "$staging/pid" || exit 1
: > "$staging/log" || exit 1
topic=${2:-}
topic=${topic//[[:cntrl:]]/ }
[[ -z "$topic" ]] || printf '%s\n' "${topic:0:255}" > "$staging/topic" || exit 1
mv -T -- "$staging" "$job" || exit 1
trap - EXIT
umask "$previous_umask"

# Keep the caller's streams separate while both tee into the same job log.
exec {out_fd}> >(tee -a -- "$job/log")
out_pid=$!
exec {err_fd}> >(tee -a -- "$job/log" >&2)
err_pid=$!
"${SHELL:-bash}" -c "$1" 1>&"$out_fd" 2>&"$err_fd"
status=$?
exec {err_fd}>&- {out_fd}>&-
wait "$err_pid" "$out_pid"
printf '%s\n' "$status" > "$job/exit"
exit "$status"

# vim: set ft=sh et ts=2 sw=2 :
