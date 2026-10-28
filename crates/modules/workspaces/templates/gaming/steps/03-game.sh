# Detached: your game (GAME_COMMAND).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$GAME_COMMAND" ] || exit 0
shimmer_hyprland_launch "$GAME_PLACE" "$GAME_COMMAND" "your game" game
