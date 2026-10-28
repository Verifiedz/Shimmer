# Supervised: stop with one clear sentence before anything opens.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

problem=$(shimmer_display_problem)
[ -z "$problem" ] || shimmer_fail "$problem"

shimmer_warn_if_place STEAM_COMMAND "$STEAM_COMMAND" STEAM_PLACE
shimmer_warn_if_place GAME_COMMAND "$GAME_COMMAND" GAME_PLACE

if [ -n "$STEAM_COMMAND" ]; then
    shimmer_has "${STEAM_COMMAND%% *}" || shimmer_fail "STEAM_COMMAND's ${STEAM_COMMAND%% *} isn't installed"
fi

if [ -n "$GAME_COMMAND" ]; then
    shimmer_has "${GAME_COMMAND%% *}" || shimmer_fail "GAME_COMMAND's ${GAME_COMMAND%% *} isn't installed"
fi

# Every *_PLACE answer: a valid "WORKSPACE:KEY" shape, and its monitor question filled in. A
# typo here should stop the launch, not silently open unplaced.
for pair in "STEAM_PLACE:$STEAM_PLACE" "GAME_PLACE:$GAME_PLACE"; do
    name=${pair%%:*}
    value=${pair#*:}
    [ -n "$value" ] || continue
    shimmer_hyprland_place_ok "$value" ||
        shimmer_fail "$name must look like 1:main or 4:second (a workspace number, a colon, then main or second), got '$value'"
    key=${value#*:}
    case "$key" in
        main) [ -n "$HYPRLAND_MAIN_MONITOR" ] || shimmer_fail "$name uses the main monitor, but HYPRLAND_MAIN_MONITOR is empty" ;;
        second) [ -n "$HYPRLAND_SECOND_MONITOR" ] || shimmer_fail "$name uses the second monitor, but HYPRLAND_SECOND_MONITOR is empty" ;;
    esac
done

echo "ok${STEAM_COMMAND:+, steam: $STEAM_COMMAND}${GAME_COMMAND:+, game: $GAME_COMMAND}"
