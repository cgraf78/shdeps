#compdef shdeps

# Zsh completion for shdeps

_shdeps_dep_names() {
  local conf_dir="${SHDEPS_CONF_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/shdeps}"
  [[ -d "$conf_dir" ]] || return
  local -a names
  names=(${(f)"$(grep -h '^[[:alpha:]]' "$conf_dir"/*.conf 2>/dev/null | awk '{print $1}')"})
  _describe -t dependencies 'dependency' names
}

_shdeps() {
  local -a commands=(
    'update:Install or update all dependencies'
    'self-update:Update shdeps itself'
    'list:List all configured dependencies with status'
    'check:Check if a specific dependency is installed'
    'prune:Remove orphaned dependencies no longer in config'
    'version:Print shdeps version'
    'help:Show help message'
  )

  local -a global_opts=(
    '(-c --config)'{-c,--config}'[Config directory or file]:config path:_directories'
    '(-f --force)'{-f,--force}'[Bypass TTL cache]'
    '(-R --reinstall)'{-R,--reinstall}'[Force reinstall all dependencies]'
    '(-q --quiet)'{-q,--quiet}'[Suppress interactive prompts]'
    '(-v --verbose)'{-v,--verbose}'[Verbose output]'
    '(-h --help)'{-h,--help}'[Show help message]'
  )

  _arguments -s \
    "${global_opts[@]}" \
    '1:command:->command' \
    '*::arg:->args'

  case "$state" in
  command)
    _describe -t commands 'shdeps command' commands
    ;;
  args)
    case "${words[1]}" in
    check)
      _arguments '1:dependency:_shdeps_dep_names'
      ;;
    prune)
      _arguments \
        '-y[Skip confirmation prompt]' \
        '--dry-run[Show what would be removed]'
      ;;
    esac
    ;;
  esac
}

_shdeps "$@"
