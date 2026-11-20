set -eu
[[ ${1:-} == log-cache-tail ]] || exit 2
shift
root="$HOME/.local/state/rworker/jobs"
[[ $# -eq 4 && "$2" =~ ^[0-9]+$ && "$4" =~ ^[0-9]+$ && "$1" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]] || exit 2
dir="$root/$1"
logfile="$dir/log"
[[ ! -f "$dir/log-path" ]] || logfile=$(cat "$dir/log-path")
[[ -f "$logfile" ]] || exit 1
# Use the full modification timestamp so a growing log can be
# recognized even when two snapshots fall in the same second.
stamp=$(LC_ALL=C stat -Lc '%Y %y' -- "$logfile")
seconds=${stamp%% *}
remainder=${stamp#*.}
timestamp="$seconds.${remainder%% *}"
if [[ "$timestamp" == "$3" || "$seconds" -lt "$4" ]]
then
  exit 0
fi
printf '%s\n' "$timestamp"
tail -c "$2" -- "$logfile"
