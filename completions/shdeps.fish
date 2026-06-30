# Fish completion for shdeps

# Disable file completions by default
complete -c shdeps -f

function __shdeps_commands
    command shdeps __api completion-commands 2>/dev/null
    or printf '%s\t%s\n' \
        update 'Install/update all dependencies' \
        self-update 'Update shdeps itself' \
        list 'List all configured dependencies with status' \
        check 'Check if a specific dependency is installed' \
        dep-root 'Print a configured dependency root directory' \
        dep-path 'Print a path below a configured dependency root' \
        dep-file 'Print a readable regular file below a dependency root' \
        dep-links 'Print public command links owned by a dependency' \
        prune 'Remove orphaned deps no longer in config' \
        version 'Print shdeps version' \
        help 'Show this help message'
end

# Helper: list dependency names through the same config loader the CLI uses.
function __shdeps_dep_names
    command shdeps __api completion-dep-names 2>/dev/null
    or __shdeps_dep_names_fallback
end

function __shdeps_dep_names_fallback
    set -l conf_dir (set -q SHDEPS_CONF_DIR; and echo $SHDEPS_CONF_DIR; or echo $HOME/.config/shdeps)
    test -d "$conf_dir"; or return
    grep -h '^[[:alpha:]]' $conf_dir/*.conf 2>/dev/null | awk '{print $1}'
end

# Condition: no subcommand yet
function __shdeps_needs_command
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for c in $cmd
        switch $c
            case -c --config
                set -e cmd[1] # skip the argument too
            case '-*'
                continue
            case '*'
                return 1
        end
    end
    return 0
end

# Condition: specific subcommand is active
function __shdeps_using_command
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for c in $cmd
        switch $c
            case -c --config
                set -e cmd[1]
            case '-*'
                continue
            case $argv[1]
                return 0
            case '*'
                return 1
        end
    end
    return 1
end

# Global options
complete -c shdeps -s c -l config -rF -d "Config directory or file"
complete -c shdeps -s f -l force -d "Bypass TTL cache"
complete -c shdeps -s R -l reinstall -d "Force reinstall all dependencies"
complete -c shdeps -s q -l quiet -d "Suppress non-result output and interactive prompts"
complete -c shdeps -s v -l verbose -d "Verbose output"
complete -c shdeps -s h -l help -d "Show help message"

# Subcommands
complete -c shdeps -n __shdeps_needs_command -a "(__shdeps_commands)"

# check: complete with dependency names
complete -c shdeps -n "__shdeps_using_command check" -a "(__shdeps_dep_names)" -d "Dependency name"
complete -c shdeps -n "__shdeps_using_command dep-root" -a "(__shdeps_dep_names)" -d "Dependency name"
complete -c shdeps -n "__shdeps_using_command dep-path" -a "(__shdeps_dep_names)" -d "Dependency name"
complete -c shdeps -n "__shdeps_using_command dep-file" -a "(__shdeps_dep_names)" -d "Dependency name"
complete -c shdeps -n "__shdeps_using_command dep-links" -a "(__shdeps_dep_names)" -d "Dependency name"

# prune options
complete -c shdeps -n "__shdeps_using_command prune" -s y -d "Skip confirmation prompt"
complete -c shdeps -n "__shdeps_using_command prune" -l dry-run -d "Show what would be removed"
