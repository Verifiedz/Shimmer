# Supervised: run GAME_PRELAUNCH_COMMAND, if set, before the game launches. Its own failure is
# a note, not a stopped launch: a one-time fixup (e.g. clearing a stale config file some games
# regenerate fresh on every start) can fail benignly on a later run once there's nothing left
# to fix, and that must never block every later activation.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$GAME_PRELAUNCH_COMMAND" ] || exit 0
if sh -c "$GAME_PRELAUNCH_COMMAND"; then
    echo "ran: $GAME_PRELAUNCH_COMMAND"
else
    msg="GAME_PRELAUNCH_COMMAND failed (continuing anyway): $GAME_PRELAUNCH_COMMAND"
    echo "note: $msg"
    shimmer_hyprland_log "$msg"
fi
