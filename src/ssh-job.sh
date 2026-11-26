# The login of `ssh --create` / `up --ssh` runs this script with the worker's
# topic, if any, as $1. It registers the session as a job, like `exec`, and
# records it with script(1) where that exists.
set -u

login_shell() {
  exec "${SHELL:-bash}" -l
}

# The worker's job directory: $RWORKER_JOBS_DIR (set by current images), the
# shared /var/lib/rworker/jobs, or the login account's own on older workers.
# The shared one is group-readable for the rworker group (the operator).
if [[ -n "${RWORKER_JOBS_DIR:-}" ]]; then root=$RWORKER_JOBS_DIR
elif [[ -d /var/lib/rworker/jobs && -w /var/lib/rworker/jobs ]]; then root=/var/lib/rworker/jobs
else root="$HOME/.local/state/rworker/jobs"
fi
if [[ "$root" == "$HOME/.local/state/rworker/jobs" ]]; then job_umask=077; else job_umask=007; fi
previous_umask=$(umask)
umask "$job_umask"
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
