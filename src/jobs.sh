set -eu
# The worker's job directory: $RWORKER_JOBS_DIR (set by current images), the
# shared /var/lib/rworker/jobs, or the login account's own on older workers.
# The shared one is group-readable for the rworker group (the operator).
if [[ -n "${RWORKER_JOBS_DIR:-}" ]]; then root=$RWORKER_JOBS_DIR
elif [[ -d /var/lib/rworker/jobs && -w /var/lib/rworker/jobs ]]; then root=/var/lib/rworker/jobs
else root="$HOME/.local/state/rworker/jobs"
fi
if [[ "$root" == "$HOME/.local/state/rworker/jobs" ]]; then job_umask=077; else job_umask=007; fi
umask "$job_umask"
mkdir -p "$root"
action=$1
shift
# A topic is one line of the tab-separated job list.
save_topic() {
  local topic=${2//[[:cntrl:]]/ }
  [[ -z "$topic" ]] || printf '%s\n' "${topic:0:255}" > "$1/topic"
}
valid() { [[ "$1" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; }
case "$action" in
  start)
    label=""
    workdir="$HOME"
    topic=""
    while [[ $# -gt 0 ]]
    do
      case "$1" in
        --name) label=$2; shift 2 ;;
        --workdir) workdir=$2; shift 2 ;;
        --topic) topic=$2; shift 2 ;;
        --) shift; break ;;
        *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
      esac
    done
    [[ $# -eq 1 ]] || { printf 'provide one command after --\n' >&2; exit 2; }
    if [[ -n "$label" ]]
    then
      valid "$label" || { printf 'invalid job name\n' >&2; exit 2; }
      dir="$root/$label"
      mkdir "$dir" || exit 1
    else
      dir=$(mktemp -d "$root/job-XXXXXXXX")
      # mktemp -d ignores the umask: share the job with the group like the rest.
      [[ "$job_umask" == 077 ]] || chmod 2770 "$dir" 2>/dev/null || chmod 0770 "$dir"
    fi
    printf '%s\n' "$1" > "$dir/command"
    printf '%s\n' "$workdir" > "$dir/workdir"
    save_topic "$dir" "$topic"
    date -u +%FT%TZ > "$dir/started"
    cat > "$dir/run" <<'RUN_JOB'
#!/usr/bin/env bash
job_dir=$1
cd "$(cat "$job_dir/workdir")" || { printf '%s\n' 1 > "$job_dir/exit"; exit 1; }
bash "$job_dir/command"
status=$?
rm -f "$job_dir/command"
printf '%s\n' "$status" > "$job_dir/exit"
exit "$status"
RUN_JOB
    nohup bash "$dir/run" "$dir" </dev/null >"$dir/log" 2>&1 &
    printf '%s\n' "$!" > "$dir/pid"
    basename "$dir"
    ;;
  register)
    job=${1:-}
    logfile=""
    topic=""
    [[ $# -eq 0 ]] || shift
    while [[ $# -ge 2 ]]
    do
      case "$1" in
        --log) logfile=$2; shift 2 ;;
        --topic) topic=$2; shift 2 ;;
        *) break ;;
      esac
    done
    [[ $# -eq 0 && -n "$logfile" ]] || { printf 'usage: register JOB --log PATH [--topic TEXT]\n' >&2; exit 2; }
    valid "$job" || { printf 'invalid job name\n' >&2; exit 2; }
    [[ "$logfile" == /* && -f "$logfile" ]] || { printf 'log must be an existing absolute file path\n' >&2; exit 2; }
    mkdir "$root/$job"
    printf '%s\n' "$logfile" > "$root/$job/log-path"
    save_topic "$root/$job" "$topic"
    date -u +%FT%TZ > "$root/$job/started"
    ;;
  jobs)
    for dir in "$root"/*
    do
      [[ -d "$dir" ]] || continue
      state=registered
      if [[ -f "$dir/exit" ]]
      then
        state="exit $(cat "$dir/exit")"
      elif [[ -f "$dir/pid" ]]
      then
        if kill -0 "$(cat "$dir/pid")" 2>/dev/null
        then
          state=running
        else
          state=unknown
        fi
      fi
      topic=""
      if [[ -f "$dir/topic" ]]
      then
        topic=$(cat "$dir/topic")
      fi
      printf '%s\t%s\t%s\t%s\n' "${dir##*/}" "$state" "$(cat "$dir/started")" "$topic"
    done
    ;;
  ps)
    if [[ "${1:-}" == --long && $# -eq 1 ]]
    then
      processes=$(ps -ww -eo pid=,ppid=,etime=,args=)
    elif [[ $# -eq 0 ]]
    then
      processes=$(ps -eo pid=,ppid=,etime=,comm=)
    else
      printf 'usage: ps [--long]\n' >&2
      exit 2
    fi
    for dir in "$root"/*
    do
      [[ -d "$dir" && -f "$dir/pid" && ! -f "$dir/exit" ]] || continue
      pid=$(cat "$dir/pid")
      [[ "$pid" =~ ^[0-9]+$ ]] || continue
      awk -v root="$pid" -v job="${dir##*/}" '
        { pids[NR]=$1; parents[NR]=$2; elapsed[NR]=$3; sub(/^[[:space:]]*[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]+/, ""); command[NR]=$0 }
        END {
          selected[root]=1
          do {
            changed=0
            for (i=1; i<=NR; i++)
              if (selected[parents[i]] && !selected[pids[i]]) {
                selected[pids[i]]=1
                changed=1
              }
          } while (changed)
          for (i=1; i<=NR; i++)
            if (selected[pids[i]])
              printf "%s\t%s\t%s\t%s\t%s\n", job, pids[i], parents[i], elapsed[i], command[i]
        }
      ' <<< "$processes"
    done
    ;;
  logs)
    job=""
    lines=200
    follow=""
    quiet=""
    worker_prefix=""
    while [[ $# -gt 0 ]]
    do
      case "$1" in
        --lines) lines=$2; shift 2 ;;
        --follow|-f) follow=1; shift ;;
        --quiet|-q) quiet=1; shift ;;
        --worker-prefix) worker_prefix=$2; shift 2 ;;
        *) [[ -z "$job" ]] || exit 2; job=$1; shift ;;
      esac
    done
    [[ "$lines" =~ ^[0-9]+$ ]] || { printf 'invalid line count\n' >&2; exit 2; }
    if [[ -n "$job" ]]
    then
      valid "$job" || exit 2
      dirs=("$root/$job")
    else
      dirs=("$root"/*)
    fi
    if [[ -n "$follow" ]]
    then
      colors=(31 32 33 34 35 36)
      pids=()
      if [[ -n "$job" ]]
      then
        dir="${dirs[0]}"
        [[ -d "$dir" ]] || { printf 'unknown job\n' >&2; exit 1; }
        logfile="$dir/log"
        [[ ! -f "$dir/log-path" ]] || logfile=$(cat "$dir/log-path")
        name=${dir##*/}
        color=${colors[0]}
        tail -n "$lines" -f -- "$logfile" | while IFS= read -r line || [[ -n "$line" ]]
        do
          printf '\033[1;%sm%s/%s\033[0m %s\n' "$color" "$worker_prefix" "$name" "$line"
        done &
        pids+=("$!")
        wait "${pids[@]}"
        exit $?
      fi

      declare -A followed=()
      index=0
      reported_waiting=""
      cleanup_tailers() {
        local pid
        for pid in "${pids[@]}"
        do
          kill "$pid" 2>/dev/null || true
        done
      }
      trap cleanup_tailers EXIT
      trap 'exit 130' INT
      trap 'exit 143' TERM
      while :
      do
        found_job=""
        for dir in "$root"/*
        do
          [[ -d "$dir" ]] || continue
          name=${dir##*/}
          [[ -n "${followed[$name]:-}" ]] && continue
          logfile="$dir/log"
          if [[ -f "$dir/log-path" ]]
          then
            logfile=$(cat "$dir/log-path")
          fi
          [[ -f "$logfile" ]] || continue
          followed["$name"]=1
          found_job=1
          color=${colors[$((index % ${#colors[@]}))]}
          tail -n "$lines" -f -- "$logfile" | while IFS= read -r line || [[ -n "$line" ]]
          do
            printf '\033[1;%sm%s/%s\033[0m %s\n' "$color" "$worker_prefix" "$name" "$line"
          done &
          pids+=("$!")
          ((index += 1))
        done
        if [[ -z "$found_job" && ${#followed[@]} -eq 0 && -z "$reported_waiting" && -z "$quiet" ]]
        then
          printf '\033[3;90m%s\033[0m \033[3mwaiting for jobs...\033[0m\n' "$(date -u +%FT%TZ)" >&2
          reported_waiting=1
        fi
        sleep 1
      done
    fi
    for dir in "${dirs[@]}"
    do
      [[ -d "$dir" ]] || { [[ -z "$job" ]] && continue; printf 'unknown job\n' >&2; exit 1; }
      logfile="$dir/log"
      [[ ! -f "$dir/log-path" ]] || logfile=$(cat "$dir/log-path")
      printf '==> %s/%s <==\n' "$worker_prefix" "${dir##*/}"
      tail -n "$lines" -- "$logfile"
    done
    ;;
esac
