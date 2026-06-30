# Bash completion for shdeps
# shellcheck disable=SC2207  # compgen output splitting is intentional

_shdeps_completion_commands() {
  command shdeps __api completion-commands 2>/dev/null | awk -F '\t' '{print $1}'
}

_shdeps_completion_commands_fallback() {
  printf '%s\n' update self-update list check dep-root dep-path dep-file dep-links prune version help
}

_shdeps_dep_names() {
  command shdeps __api completion-dep-names 2>/dev/null
}

_shdeps() {
  local cur prev words cword
  _init_completion || return

  local commands
  commands="$(_shdeps_completion_commands)"
  [[ -n "$commands" ]] || commands="$(_shdeps_completion_commands_fallback)"
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
    check | dep-root | dep-path | dep-file | dep-links)
      # Ask the Rust CLI for names so completion follows the real config
      # grammar, canonicalization, dedupe, and safety filters.
      local names
      names="$(_shdeps_dep_names)"
      COMPREPLY=($(compgen -W "$names" -- "$cur"))
      ;;
    prune)
      local prune_opts="-y --dry-run"
      COMPREPLY=($(compgen -W "$prune_opts" -- "$cur"))
      ;;
  esac
}

complete -F _shdeps shdeps
