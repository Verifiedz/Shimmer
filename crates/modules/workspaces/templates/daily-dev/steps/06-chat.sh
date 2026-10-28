# Detached: your chat app (DISCORD_COMMAND).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$DISCORD_COMMAND" ] || exit 0
shimmer_hyprland_launch "$CHAT_PLACE" "$DISCORD_COMMAND" \
    "the $DISCORD_COMMAND window (it keeps one program running; opening it again just focuses it)" chat
