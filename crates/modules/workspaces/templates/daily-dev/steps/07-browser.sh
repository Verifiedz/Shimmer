# Detached: your browser (BROWSER_COMMAND), optionally with startup tabs (BROWSER_URLS).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

[ -n "$BROWSER_COMMAND" ] || exit 0

command=$BROWSER_COMMAND
if [ -n "$BROWSER_URLS" ]; then
    # BROWSER_URLS is space-separated on purpose: split it into words.
    # shellcheck disable=SC2086
    set -- $BROWSER_URLS
    for url in "$@"; do
        command="$command $(shimmer_quote "$url")"
    done
fi
shimmer_hyprland_launch "$BROWSER_PLACE" "$command" "the $BROWSER_COMMAND window"
