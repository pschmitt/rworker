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
  else
    if [[ "$action" == setup ]]
    then
      case "${COMP_WORDS[2]}" in
        niks3-token) spec=$(sed '/^flag setup /{ /^flag setup --force /!d; }' <<< "$spec") ;;
        register-log-file) spec=$(sed '/^flag setup /{ /^flag setup --log=/!d; }' <<< "$spec") ;;
        netbird|tailscale) spec=$(sed '/^flag setup /d' <<< "$spec") ;;
        *) spec=$(sed '/^flag setup --force /d; /^flag setup --log=/d' <<< "$spec") ;;
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
      elif [[ "$w" == --lines || "$w" == --topic || "$w" == --agent || "$w" == --agent-id || "$w" == --redact-regex ]]
      then
        skipval=1
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
  mapfile -t COMPREPLY < <(compgen -W "$words" -- "$cur")
}

complete -F _rworker rworker

# vim: set ft=sh et ts=2 sw=2 :
