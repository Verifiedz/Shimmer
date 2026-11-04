# Detached: Steam (STEAM_COMMAND).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$STEAM_COMMAND" ] || exit 0
shimmer_hyprland_launch "$STEAM_PLACE" "$STEAM_COMMAND" "the Steam window" steam
