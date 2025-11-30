# Dynamic branch completion for open/merge/remove commands
_workmux_branches() {
    workmux __complete-branches 2>/dev/null
}

# Dynamic git branch completion for add command
_workmux_git_branches() {
    workmux __complete-git-branches 2>/dev/null
}

# Wrapper that adds dynamic branch completion
_workmux_dynamic() {
    local cur prev words cword

    # Use _init_completion if available, otherwise fall back to manual parsing
    if declare -F _init_completion >/dev/null 2>&1; then
        _init_completion || return
    else
        COMPREPLY=()
        cur="${COMP_WORDS[COMP_CWORD]}"
        prev="${COMP_WORDS[COMP_CWORD-1]}"
        words=("${COMP_WORDS[@]}")
        cword=$COMP_CWORD
    fi

    # Check if we're completing a branch argument for specific commands
    if [[ ${cword} -ge 2 ]]; then
        local cmd="${words[1]}"
        case "$cmd" in
            open|merge|remove|rm|path)
                # If not typing a flag, complete with branches
                if [[ "$cur" != -* ]]; then
                    COMPREPLY=($(compgen -W "$(_workmux_branches)" -- "$cur"))
                    return
                fi
                ;;
            add)
                # If not typing a flag, complete with git branches
                if [[ "$cur" != -* ]]; then
                    COMPREPLY=($(compgen -W "$(_workmux_git_branches)" -- "$cur"))
                    return
                fi
                ;;
        esac
    fi

    # Fall back to generated completions
    _workmux
}

complete -F _workmux_dynamic -o bashdefault -o default workmux
