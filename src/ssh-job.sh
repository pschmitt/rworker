# The login of `ssh --create` / `up --ssh` runs this script with the worker's
# topic, if any, as $1. It registers the session as a job, like `exec`, and
# records it with script(1) where that exists.
set -u

login_shell() {
  exec "${SHELL:-bash}" -l
}

root="$HOME/.local/state/rworker/jobs"
previous_umask=$(umask)
umask 077
mkdir -p -- "$root" || login_shell
job="$root/ssh"
[[ ! -e "$job" ]] || job="$root/ssh-$$"
staging=$(mktemp -d "$root/.ssh.XXXXXX") || login_shell
date -u +%FT%TZ > "$staging/started" &&
  printf '%s\n' "$$" > "$staging/pid" &&
  : > "$staging/log" || { rm -rf -- "$staging"; login_shell; }
topic=${1:-}
topic=${topic//[[:cntrl:]]/ }
[[ -z "$topic" ]] || printf '%s\n' "${topic:0:255}" > "$staging/topic"
mv -T -- "$staging" "$job" || { rm -rf -- "$staging"; login_shell; }
umask "$previous_umask"

if command -v script >/dev/null 2>&1
then
  script -qfec "${SHELL:-bash} -l" "$job/log"
else
  "${SHELL:-bash}" -l
fi
status=$?
printf '%s\n' "$status" > "$job/exit"
exit "$status"

# vim: set ft=sh et ts=2 sw=2 :
