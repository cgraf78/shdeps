#compdef shdeps

# Zsh completion for shdeps

_shdeps_command_specs() {
  local name description
  command shdeps __api completion-commands 2>/dev/null | while IFS=$'\t' read -r name description; do
    [[ -n "$name" ]] && printf '%s:%s\n' "$name" "$description"
  done
}

_shdeps_command_specs_fallback() {
  printf '%s\n' \
    'update:Install/update all dependencies' \
    'self-update:Update shdeps itself' \
    'list:List all configured dependencies with status' \
    'check:Check if a specific dependency is installed' \
    'dep-root:Print a configured dependency root directory' \
    'dep-path:Print a path below a configured dependency root' \
    'dep-file:Print a readable regular file below a dependency root' \
    'dep-links:Print public command links owned by a dependency' \
    'prune:Remove orphaned deps no longer in config' \
    'version:Print shdeps version' \
    'help:Show this help message'
}

_shdeps_dep_names() {
  local api_names
  local -a names
  # Use the same Rust config loader as the CLI instead of a completion-local
  # parser; repo names, `.git` canonicalization, and duplicate handling matter.
  if api_names="$(command shdeps __api completion-dep-names 2>/dev/null)"; then
    names=(${(f)api_names})
  else
    local conf_dir="${SHDEPS_CONF_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/shdeps}"
    if [[ -d "$conf_dir" ]]; then
      names=(${(f)"$(grep -h '^[[:alpha:]]' "$conf_dir"/*.conf 2>/dev/null | awk '{print $1}')"})
    fi
  fi
  _describe -t dependencies 'dependency' names
}

_shdeps() {
  local -a commands
  commands=(${(f)"$(_shdeps_command_specs)"})
  ((${#commands[@]} > 0)) || commands=(${(f)"$(_shdeps_command_specs_fallback)"})

  local -a global_opts=(
    '(-c --config)'{-c,--config}'[Config directory or file]:config path:_directories'
    '(-f --force)'{-f,--force}'[Bypass TTL cache]'
    '(-R --reinstall)'{-R,--reinstall}'[Force reinstall all dependencies]'
    '(-q --quiet)'{-q,--quiet}'[Suppress non-result output and interactive prompts]'
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
        check | dep-root | dep-path | dep-file | dep-links)
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
