# Shimmer completion for zsh (#188). Load it from ~/.zshrc, after compinit:
#   source <(shimmer completions zsh)
# Tab completes commands, options, and your own workspaces, collections, records and templates,
# in titled groups coloured with the Shimmer palette. NO_COLOR turns the colours off.

_shimmer() {
    local -a out group slashed
    local entry tag title word desc ret=1
    out=("${(@f)$(@EXE@ __complete "${(@)words[1,CURRENT]}" 2>/dev/null)}")
    for entry in "${out[@]}" '>'; do
        if [[ $entry == '>'* ]]; then
            if [[ -n $tag ]]; then
                (( $#group )) && _describe -t "$tag" "$title" group && ret=0
                # `lc/`: a collection, its record still to come; no space after it.
                (( $#slashed )) && _describe -t "$tag" "$title" slashed -S '' && ret=0
            fi
            group=() slashed=()
            entry=${entry#>}
            tag=${entry%%$'\t'*} title=${entry#*$'\t'}
            if [[ $tag == files ]]; then
                _files && ret=0
                tag=''
            fi
            continue
        fi
        [[ -z $entry ]] && continue
        word=${entry%%$'\t'*} desc=''
        [[ $entry == *$'\t'* ]] && desc=${entry#*$'\t'}
        word=${word//:/\\:}
        if [[ $word == */ ]]; then
            slashed+=("$word${desc:+:$desc}")
        else
            group+=("$word${desc:+:$desc}")
        fi
    done
    return ret
}

if (( $+functions[compdef] )); then
    compdef _shimmer @NAMES@
else
    print -u2 "shimmer: load completion after compinit (autoload -Uz compinit && compinit)"
fi

# Colours, in the Shimmer palette (#189), only for Shimmer's own commands.
if [[ -z $NO_COLOR ]]; then
    zmodload -i zsh/complist 2>/dev/null
    zstyle ':completion:*:*:@PATTERN@:*' group-name ''
    zstyle ':completion:*:*:@PATTERN@:*:descriptions' format $'%{\e[@HEADING_CODE@m%}%d%{\e[0m%}'
    zstyle ':completion:*:*:@PATTERN@:*:options' list-colors \
        '=(#b)([^ ]#)( *)=0=@HINT_CODE@=@HINT_CODE@' '=*=@HINT_CODE@'
    zstyle ':completion:*:*:@PATTERN@:*' list-colors \
        '=(#b)([^ ]#)( *)=0=@NAME_CODE@=@HINT_CODE@' '=*=@NAME_CODE@'
fi
