# Shimmer completion for bash (#188). Load it from ~/.bashrc:
#   source <(shimmer completions bash)
# Tab completes commands, options, and your own workspaces, collections, records and templates.

_shimmer() {
    # The words up to the cursor, split here rather than by COMP_WORDBREAKS, which would cut
    # `lc/two-sum` and `--lane=default` apart.
    local line=${COMP_LINE:0:COMP_POINT}
    local -a words
    read -ra words <<< "$line"
    [[ $line == *[[:space:]] || ${#words[@]} -eq 0 ]] && words+=("")
    local cur=${words[${#words[@]}-1]}
    COMPREPLY=()
    local entry word files='' words_seen=0 slashed=0
    while IFS= read -r entry; do
        case $entry in
            '>files'*) files=1 ;;
            '>'*) ;;
            *)
                word=${entry%%$'\t'*}
                COMPREPLY+=("$word")
                (( words_seen++ ))
                [[ $word == */ ]] && (( slashed++ ))
                ;;
        esac
    done < <(@EXE@ __complete "${words[@]}" 2>/dev/null)
    if [[ -n $files ]]; then
        local f
        while IFS= read -r f; do COMPREPLY+=("$f"); done < <(compgen -f -- "$cur")
    fi
    # bash's nospace is for the whole list: only when every suggestion is a `collection/`, so
    # picking a plain word still gets its space.
    if (( words_seen > 0 && slashed == words_seen )) && type compopt &>/dev/null; then
        compopt -o nospace 2>/dev/null
    fi
}

complete -F _shimmer @NAMES@
