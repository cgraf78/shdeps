# Fish completion for shdeps

# Disable file completions by default
complete -c shdeps -f

# Helper: list dependency names from config
function __shdeps_dep_names
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
complete -c shdeps -s q -l quiet -d "Suppress interactive prompts"
complete -c shdeps -s v -l verbose -d "Verbose output"
complete -c shdeps -s h -l help -d "Show help message"

# Subcommands
complete -c shdeps -n __shdeps_needs_command -a update -d "Install/update all dependencies"
complete -c shdeps -n __shdeps_needs_command -a self-update -d "Update shdeps itself"
complete -c shdeps -n __shdeps_needs_command -a list -d "List all configured dependencies"
complete -c shdeps -n __shdeps_needs_command -a check -d "Check if a dependency is installed"
complete -c shdeps -n __shdeps_needs_command -a prune -d "Remove orphaned dependencies"
complete -c shdeps -n __shdeps_needs_command -a version -d "Print shdeps version"
complete -c shdeps -n __shdeps_needs_command -a help -d "Show help message"

# check: complete with dependency names
complete -c shdeps -n "__shdeps_using_command check" -a "(__shdeps_dep_names)" -d "Dependency name"

# prune options
complete -c shdeps -n "__shdeps_using_command prune" -s y -d "Skip confirmation prompt"
complete -c shdeps -n "__shdeps_using_command prune" -l dry-run -d "Show what would be removed"
