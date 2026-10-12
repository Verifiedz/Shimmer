# Shimmer completion for fish (#188). Load it from ~/.config/fish/config.fish:
#   shimmer completions fish | source
# Tab completes commands, options, and your own workspaces, collections, records and templates,
# each with what it is. fish colours the line as you type with your fish theme.

function __shimmer_complete
    set -l words (commandline -opc) (commandline -ct)
    @EXE@ __complete $words 2>/dev/null | while read -l entry
        switch $entry
            case '>files*'
                __fish_complete_path (commandline -ct)
            case '>*'
            case '*'
                echo $entry
        end
    end
end

for name in @NAMES@
    complete -c $name -f -a '(__shimmer_complete)'
end
