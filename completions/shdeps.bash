# Bash completion for shdeps
# shellcheck disable=SC2207  # compgen output splitting is intentional

_shdeps() {
  local cur prev words cword
  _init_completion || return

  local commands="update self-update list check prune version help"
  local global_opts="-c --config -f --force -R --reinstall -q --quiet -v --verbose -h --help"

  # Find the subcommand (skip options and their arguments)
  local cmd=""
  local i
  for ((i = 1; i < cword; i++)); do
    case "${words[i]}" in
    -c | --config)
      ((i++))
      ;;
    -*)
      ;;
    *)
      cmd="${words[i]}"
      break
      ;;
    esac
  done

  # Complete option arguments
  case "$prev" in
  -c | --config)
    _filedir -d
    return
    ;;
  esac

  # No subcommand yet — complete commands and global options
  if [[ -z "$cmd" ]]; then
    if [[ "$cur" == -* ]]; then
      COMPREPLY=($(compgen -W "$global_opts" -- "$cur"))
    else
      COMPREPLY=($(compgen -W "$commands" -- "$cur"))
    fi
    return
  fi

  # Subcommand-specific completions
  case "$cmd" in
  check)
    # Complete with dependency names from config files
    local conf_dir="${SHDEPS_CONF_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/shdeps}"
    if [[ -d "$conf_dir" ]]; then
      local names
      names=$(grep -h '^[[:alpha:]]' "$conf_dir"/*.conf 2>/dev/null | awk '{print $1}')
      COMPREPLY=($(compgen -W "$names" -- "$cur"))
    fi
    ;;
  prune)
    local prune_opts="-y --dry-run"
    COMPREPLY=($(compgen -W "$prune_opts" -- "$cur"))
    ;;
  esac
}

complete -F _shdeps shdeps
