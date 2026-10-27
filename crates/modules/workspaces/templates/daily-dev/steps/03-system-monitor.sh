# Detached: a system monitor (SYSTEM_MONITOR_COMMAND), in its own terminal.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

[ -n "$SYSTEM_MONITOR_COMMAND" ] || exit 0

term=${TERMINAL_APP:-auto}
[ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
if [ -n "$MONITORS_PLACE" ] && [ "$term" = "kitty" ]; then
    shimmer_hyprland_launch "$MONITORS_PLACE" "kitty $SYSTEM_MONITOR_COMMAND" "the system monitor window"
else
    if [ -n "$MONITORS_PLACE" ]; then
        msg="MONITORS_PLACE is set, but TERMINAL_APP resolves to '$term', not kitty: opening the system monitor without placement"
        echo "note: $msg"
        shimmer_daily_dev_log "$msg"
    fi
    shimmer_open_terminal "${TERMINAL_APP:-auto}" "$HOME" "$SYSTEM_MONITOR_COMMAND"
fi
