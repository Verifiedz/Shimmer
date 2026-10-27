# Detached: a GPU monitor (GPU_MONITOR_COMMAND), in its own terminal.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

[ -n "$GPU_MONITOR_COMMAND" ] || exit 0

term=${TERMINAL_APP:-auto}
[ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
if [ -n "$MONITORS_PLACE" ] && [ "$term" = "kitty" ]; then
    shimmer_hyprland_launch "$MONITORS_PLACE" "kitty $GPU_MONITOR_COMMAND" "the GPU monitor window"
else
    [ -n "$MONITORS_PLACE" ] && echo "note: MONITORS_PLACE is set, but TERMINAL_APP resolves to '$term', not kitty: opening the GPU monitor without placement"
    shimmer_open_terminal "${TERMINAL_APP:-auto}" "$HOME" "$GPU_MONITOR_COMMAND"
fi
