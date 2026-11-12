# Detached: your game. GAME_LIBRARY, if set, shows a picker (GAME_PICKER) over several games
# instead of launching the fixed GAME_COMMAND. Given a long window-matching poll (60s, not the
# usual ~8s): a game, especially launched through Steam/Proton, commonly takes far longer than
# other apps to map its first window, and a missed match means no address ever gets recorded
# for workspaces stop to close later.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

if [ -n "$GAME_LIBRARY" ]; then
    command=$(shimmer_pick "${GAME_PICKER:-auto}" "$GAME_LIBRARY" "Play") || {
        shimmer_hyprland_log "game: no title picked (cancelled, or no picker available)"
        exit 0
    }
else
    command=$GAME_COMMAND
fi
[ -n "$command" ] || exit 0
shimmer_hyprland_launch "$GAME_PLACE" "$command" "your game" game 120
