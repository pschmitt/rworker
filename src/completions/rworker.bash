# bash completion for rworker (`rworker completions bash`). Actions, flags and values come
# from `rworker __spec`, worker names from `rworker __workers` (the cached
# last ls, refreshed in the background), so TAB never waits on the network.

_rworker_workers() {
  rworker __workers "$action" 2>/dev/null | cut -f1
}

# Topics, agents and agent ids of the cached workers (rworker __values SET)
# that start with CUR, shell-quoted (they may contain spaces).
_rworker_values() {
  local set="$1" cur="$2" value
  COMPREPLY=()
  while IFS=$'\t' read -r value _
  do
    if [[ -n "$value" && "$value" == "$cur"* ]]
    then
      COMPREPLY+=("$(printf '%q' "$value")")
    fi
  done < <(rworker __values "$set" 2>/dev/null)
}

# Complete a structured filter value, accounting for Bash's colon word break.
_rworker_search_value() {
  local token="$1" cur="$2" key="${1%%:*}" typed="${1#*:}" set value candidate head
  local invert="" quote=""
  case "$key" in
    topic) set=topic-re ;;
    agent) set=agent-re ;;
    agent-id) set=id-re ;;
    *) return 1 ;;
  esac
  if [[ "$typed" == \"* || "$typed" == \'* ]]
  then
    quote="${typed:0:1}"
    typed="${typed:1}"
  fi
  if [[ "$typed" == !* ]]
  then
    invert="!"
    typed="${typed:1}"
  fi
  head="${token%"$cur"}"
  COMPREPLY=()
  while IFS=$'\t' read -r value _
  do
    if [[ -n "$value" && "$value" == "$typed"* ]]
    then
      candidate="$key:$invert$value"
      candidate="${candidate#"$head"}"
      if [[ -n "$quote" ]]
      then
        COMPREPLY+=("$candidate")
      else
        COMPREPLY+=("$(printf '%q' "$candidate")")
      fi
    fi
  done < <(rworker __values "$set" 2>/dev/null)
}

_rworker() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  local prev="${COMP_WORDS[COMP_CWORD-1]}"
  local action="${COMP_WORDS[1]:-}"
  local spec words="" line flag
  spec=$(rworker __spec 2>/dev/null)

  if [[ $COMP_CWORD -gt 1 ]]
  then
    action=$(rworker __action "$action" 2>/dev/null) || return
  fi
  # COMP_WORDS splits at colons by default; recover the whole current term.
  if [[ ( "$action" == machines || "$action" == logs ) && $COMP_CWORD -ge 2 ]]
  then
    local before="${COMP_LINE:0:COMP_POINT}" token prior
    token="${before##*[[:space:]]}"
    before="${before%"$token"}"
    before="${before%"${before##*[![:space:]]}"}"
    prior="${before##*[[:space:]]}"
    case "$prior" in
      --lines|--topic|--agent|--agent-id|--redact-regex|--filter|--grep|-g|-o|-n|--interval) ;;
      *)
        case "$token" in
          topic:*|agent:*|agent-id:*) _rworker_search_value "$token" "$cur"; return ;;
        esac
        ;;
    esac
  fi

  if [[ $COMP_CWORD -eq 1 ]] || [[ "$action" == help && $COMP_CWORD -eq 2 ]]
  then
    words=$(sed -n -e 's/^action \([^ ]*\) .*/\1/p' -e 's/^alias \([^ ]*\) .*/\1/p' <<< "$spec")
  elif [[ $COMP_CWORD -eq 2 && "$cur" != -* ]] && grep -q -E "^worker( .*)? ${action}( |\$)" <<< "$spec"
  then
    words=$(_rworker_workers)
  elif [[ "$action" == cp ]]
  then
    if [[ "$cur" != *:* ]]
    then
      mapfile -t COMPREPLY < <(compgen -W "$(_rworker_workers | sed 's/$/:/')" -- "$cur"; compgen -f -- "$cur")
      compopt -o nospace 2>/dev/null
    fi
    return
  elif [[ "$action" == sync && $COMP_CWORD -eq 3 ]]
  then
    mapfile -t COMPREPLY < <(compgen -d -- "$cur")
    return
  elif [[ "$action" == setup && $COMP_CWORD -eq 2 ]]
  then
    words=$(sed -n 's/^values setup-topic //p' <<< "$spec")
  elif [[ "$action" == setup && $COMP_CWORD -eq 3 && "$cur" != -* ]]
  then
    words=$(_rworker_workers)
    if [[ "${COMP_WORDS[2]}" == nix ]]
    then
      words="$(sed -n 's/^values setup-nix-topic //p' <<< "$spec") $words"
    fi
  elif [[ "$action" == setup && "${COMP_WORDS[2]}" == nix &&
          $COMP_CWORD -eq 4 && "$cur" != -* &&
          " cache auth builder upload-blob -- " == *" ${COMP_WORDS[3]} "* ]]
  then
    words=$(_rworker_workers)
  elif [[ "$action" == setup && "${COMP_WORDS[2]}" == nix &&
          "${COMP_WORDS[3]}" == upload-blob && $COMP_CWORD -gt 4 ]]
  then
    if [[ "$prev" == --name ]]
    then
      return
    elif [[ "$cur" == -* ]]
    then
      words='--name --help --'
    else
      mapfile -t COMPREPLY < <(compgen -f -- "$cur")
      return
    fi
  else
    if [[ "$action" == setup ]]
    then
      local setup_topic="${COMP_WORDS[2]}"
      if [[ "$setup_topic" == nix && $COMP_CWORD -gt 3 ]]
      then
        case "${COMP_WORDS[3]}" in
          cache|builder) setup_topic="nix-${COMP_WORDS[3]}" ;;
          auth|upload-blob) setup_topic="${COMP_WORDS[3]}" ;;
        esac
      fi
      case "$setup_topic" in
        auth) spec=$(sed -n '/^flag setup /!p; /^flag setup \(--dry-run \|-n \|--auth-host=\)/p' <<< "$spec") ;;
        upload-blob) spec=$(sed -n '/^flag setup /!p; /^flag setup --name=/p' <<< "$spec") ;;
        niks3-token) spec=$(sed '/^flag setup /{ /^flag setup --force /!d; }' <<< "$spec") ;;
        register-log-file) spec=$(sed '/^flag setup /{ /^flag setup --\(log\|topic\)=/!d; }' <<< "$spec") ;;
        netbird|tailscale) spec=$(sed '/^flag setup /d' <<< "$spec") ;;
        nix-cache) spec=$(sed '/^flag setup /{ /^flag setup \(--dry-run\|-n\) /!d; }' <<< "$spec") ;;
        nix-builder) spec=$(sed -n '/^flag setup /!p; /^flag setup \(--profile\|--arch\|--pool\|--gateway\|--identity\|-i\|--host-key\|--system\|--jobs\|--features\)=/p' <<< "$spec") ;;
        *) spec=$(sed '/^flag setup --force /d; /^flag setup --name=/d; /^flag setup --\(log\|topic\)=/d; /^flag setup \(--profile\|--arch\|--pool\|--gateway\|--identity\|-i\|--host-key\|--system\|--jobs\|--features\)=/d' <<< "$spec") ;;
      esac
    fi
    # ssh --create takes up's flags.
    if [[ "$action" == ssh && "${COMP_WORDS[2]:-}" == --create ]]
    then
      action=up
    fi
    # value of the previous flag?
    while read -r line
    do
      flag="${line%% *}"
      if [[ "$flag" == "${prev}=path" ]]
      then
        mapfile -t COMPREPLY < <(compgen -f -- "$cur")
        return
      elif [[ "$flag" == "${prev}="* ]] && grep -q -E "^dynamic( .*)? ${flag#*=}( |\$)" <<< "$spec"
      then
        _rworker_values "${flag#*=}" "$cur"
        return
      elif [[ "$flag" == "${prev}="* ]]
      then
        words=$(sed -n "s/^values ${flag#*=} //p" <<< "$spec")
        mapfile -t COMPREPLY < <(compgen -W "$words" -- "$cur")
        return
      fi
    done < <(sed -n "s/^flag ${action} //p" <<< "$spec")
    words="$(sed -n "s/^flag ${action} \([^ =]*\).*/\1/p" <<< "$spec") --help"
    if [[ "$action" != ssh ]]
    then
      words+=" $(sed -n 's/^global \([^ ]*\) .*/\1/p' <<< "$spec")"
    fi
  fi
  if [[ "$action" == tui && "$cur" != -* ]]
  then
    local i=2
    while ((i < COMP_CWORD))
    do
      case "${COMP_WORDS[i]}" in
        -n|--interval) ((i += 2)) ;;
        -*) ((i += 1)) ;;
        *) break ;;
      esac
    done
    if ((i == COMP_CWORD))
    then
      words+=" $(sed -n 's/^values tui-view //p' <<< "$spec")"
    fi
  fi
  if [[ "$action" == logs && $COMP_CWORD -ge 3 ]]
  then
    # logs [FLAGS] [WORKER [JOB]]: the worker is the first word that is neither
    # a flag nor a flag's value; complete it, then its jobs.
    local i w skipval=0 worker=""
    for ((i = 2; i < COMP_CWORD; i++))
    do
      w="${COMP_WORDS[i]}"
      if ((skipval))
      then
        skipval=0
      elif [[ "$w" == --lines || "$w" == --topic || "$w" == --agent || "$w" == --agent-id || "$w" == --redact-regex || "$w" == --filter || "$w" == --grep || "$w" == -g ]]
      then
        skipval=1
      elif [[ "$w" == topic:* || "$w" == agent:* || "$w" == agent-id:* ]]
      then
        continue
      elif [[ ( "$w" == topic || "$w" == agent || "$w" == agent-id ) && "${COMP_WORDS[i+1]:-}" == : ]]
      then
        ((i += 2))
        while [[ "${COMP_WORDS[i+1]:-}" == : ]]
        do
          ((i += 2))
        done
      elif [[ "$w" != -* ]]
      then
        worker="$w"
        break
      fi
    done
    if [[ -z "$worker" && "$cur" != -* && "$prev" != --lines && "$prev" != --topic && "$prev" != --agent && "$prev" != --agent-id ]]
    then
      words+=" $(_rworker_workers)"
    elif [[ -n "$worker" ]]
    then
      words+=" $(rworker __jobs "$worker" 2>/dev/null)"
    fi
  fi
  if [[ ( "$action" == machines || ( "$action" == logs && -z "${worker:-}" ) ) && "$cur" != -* ]]
  then
    case "${prev}" in
      --lines|--topic|--agent|--agent-id|--redact-regex|--filter|--grep|-g|-o|-n|--interval) ;;
      *) words+=" $(sed -n 's/^values search-filter //p' <<< "$spec")" ;;
    esac
  fi
  mapfile -t COMPREPLY < <(compgen -W "$words" -- "$cur")
  if ((${#COMPREPLY[@]}))
  then
    local all_filters=1 result
    for result in "${COMPREPLY[@]}"
    do
      case "$result" in
        topic:|agent:|agent-id:) ;;
        *) all_filters=0; break ;;
      esac
    done
    ((all_filters)) && compopt -o nospace 2>/dev/null
  fi
}

complete -F _rworker rworker

# vim: set ft=sh et ts=2 sw=2 :
