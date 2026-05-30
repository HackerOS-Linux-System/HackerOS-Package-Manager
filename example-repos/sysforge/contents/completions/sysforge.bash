# sysforge bash completion
# Install: source /usr/share/bash-completion/completions/sysforge
# or add to ~/.bashrc

_sysforge_complete() {
    local cur prev words
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    local commands="cpu mem net disk proc trace audit container dashboard export --help --version"

    case "$prev" in
        sysforge|sfctl)
            COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
            return ;;
        cpu)
            COMPREPLY=( $(compgen -W "0.5 1 2 5" -- "$cur") )
            return ;;
        container)
            COMPREPLY=( $(compgen -W "list namespaces cgroups" -- "$cur") )
            return ;;
        export)
            COMPREPLY=( $(compgen -W "json prometheus" -- "$cur") )
            return ;;
        trace|proc)
            # Complete with running PIDs
            local pids
            pids=$(ls /proc | grep '^[0-9]' | head -20)
            COMPREPLY=( $(compgen -W "$pids" -- "$cur") )
            return ;;
    esac

    COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
}

complete -F _sysforge_complete sysforge
complete -F _sysforge_complete sfctl
