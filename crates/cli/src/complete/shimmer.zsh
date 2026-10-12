# Shimmer completion for zsh (#188). Load it from ~/.zshrc, after compinit:
#   source <(shimmer completions zsh)
# Tab completes commands, options, and your own workspaces, collections, records and templates,
# in titled groups coloured with the Shimmer palette. The command line is coloured as you type:
# commands you can run in the name colour, a misspelt one in the error colour, options grey.
# NO_COLOR turns the colours off.

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

# The command line as you type it: `shimmer`, then each command and subcommand you can run in
# the name colour (a misspelt one in the error colour), and options grey.
typeset -ga _shimmer_top=(@TOP@)
typeset -gA _shimmer_subs=(
@SUBS@
)
typeset -ga _shimmer_names=(@NAMES@)

_shimmer_highlight() {
    region_highlight=("${(@)region_highlight:#*memo=shimmer}")
    [[ -n $NO_COLOR ]] && return
    local buffer=$BUFFER n=${#BUFFER} i=0 start word index=0 top='' takes_value=''
    local -a marks
    while (( i < n )); do
        while (( i < n )) && [[ ${buffer:$i:1} == [[:space:]] ]]; do (( i++ )); done
        (( i >= n )) && break
        start=$i
        while (( i < n )) && [[ ${buffer:$i:1} != [[:space:]] ]]; do (( i++ )); done
        word=${buffer:$start:$((i - start))}
        if (( index == 0 )); then
            # Only a Shimmer command line: `shimmer`, a path to it, or a pack's alias.
            if [[ ${word:t} == shimmer ]]; then
                marks+=("$start $i @HEADING@")
            elif (( ${_shimmer_names[(Ie)$word]} )); then
                marks+=("$start $i @NAME@")
                top=alias
            else
                return
            fi
        elif [[ -n $takes_value ]]; then
            takes_value=''
        elif [[ $word == -* ]]; then
            marks+=("$start $i @HINT@")
            [[ -z $top && ( $word == --socket || $word == --pack ) ]] && takes_value=1
        elif [[ -z $top ]]; then
            if (( ${_shimmer_top[(Ie)$word]} || ${_shimmer_names[(Ie)$word]} )); then
                marks+=("$start $i @NAME@")
            else
                marks+=("$start $i @ERROR@")
            fi
            top=$word
        elif [[ -n ${_shimmer_subs[$top]} && $top != done ]]; then
            if (( ${${(s: :)_shimmer_subs[$top]}[(Ie)$word]} )); then
                marks+=("$start $i @NAME@")
            else
                marks+=("$start $i @ERROR@")
            fi
            top=done
        fi
        (( index++ ))
    done
    local mark
    for mark in $marks; do
        region_highlight+=("$mark memo=shimmer")
    done
}

# Colouring the line needs zsh 5.9 (region_highlight's memo=, so it never clears another
# highlighter's colours). On an older zsh, Tab completion still works, just without it.
autoload -Uz is-at-least add-zle-hook-widget 2>/dev/null
if is-at-least 5.9; then
    add-zle-hook-widget line-pre-redraw _shimmer_highlight
fi
