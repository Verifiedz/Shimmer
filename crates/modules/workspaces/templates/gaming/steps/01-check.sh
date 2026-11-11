# Supervised: stop with one clear sentence before anything opens.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

problem=$(shimmer_display_problem)
[ -z "$problem" ] || shimmer_fail "$problem"

shimmer_warn_if_place STEAM_COMMAND "$STEAM_COMMAND" STEAM_PLACE
shimmer_warn_if_place GAME_COMMAND "$GAME_COMMAND" GAME_PLACE
shimmer_warn_if_place SYSTEM_MONITOR_COMMAND "$SYSTEM_MONITOR_COMMAND" MONITORS_PLACE
shimmer_warn_if_place GPU_MONITOR_COMMAND "$GPU_MONITOR_COMMAND" MONITORS_PLACE
shimmer_warn_if_place BROWSER_COMMAND "$BROWSER_COMMAND" BROWSER_PLACE

if [ -n "$STEAM_COMMAND" ]; then
    shimmer_has "${STEAM_COMMAND%% *}" || shimmer_fail "STEAM_COMMAND's ${STEAM_COMMAND%% *} isn't installed"
fi

if [ -n "$GAME_PRELAUNCH_COMMAND" ]; then
    shimmer_has "${GAME_PRELAUNCH_COMMAND%% *}" ||
        shimmer_fail "GAME_PRELAUNCH_COMMAND's ${GAME_PRELAUNCH_COMMAND%% *} isn't installed"
fi

if [ -n "$GAME_LIBRARY" ]; then
    shimmer_has_picker "${GAME_PICKER:-auto}" ||
        shimmer_fail "GAME_LIBRARY is set, but no picker is available (install rofi, wofi or fuzzel, or set GAME_PICKER)"
    (
        IFS='|'
        set -f
        for entry in $GAME_LIBRARY; do
            label=${entry%%=*}
            cmd=${entry#*=}
            [ -n "$label" ] && [ "$label" != "$entry" ] || shimmer_fail "GAME_LIBRARY entry '$entry' isn't Label=command"
            shimmer_has "${cmd%% *}" || shimmer_fail "GAME_LIBRARY's '$label' (${cmd%% *}) isn't installed"
        done
    ) || exit 1
elif [ -n "$GAME_COMMAND" ]; then
    shimmer_has "${GAME_COMMAND%% *}" || shimmer_fail "GAME_COMMAND's ${GAME_COMMAND%% *} isn't installed"
fi

if [ -n "$VM_NAME" ]; then
    shimmer_has virt-manager || shimmer_fail "VM_NAME is set, but virt-manager isn't installed"
fi

if [ -n "$SYSTEM_MONITOR_COMMAND" ] || [ -n "$GPU_MONITOR_COMMAND" ]; then
    shimmer_has_terminal "${TERMINAL_APP:-none}" || shimmer_fail "the terminal '$TERMINAL_APP' isn't installed here"
    [ "${TERMINAL_APP:-none}" != "none" ] ||
        shimmer_fail "TERMINAL_APP is none, but a monitor command is set: set TERMINAL_APP to auto or a terminal"
fi

if [ -n "$SYSTEM_MONITOR_COMMAND" ]; then
    shimmer_has "${SYSTEM_MONITOR_COMMAND%% *}" ||
        shimmer_fail "SYSTEM_MONITOR_COMMAND's ${SYSTEM_MONITOR_COMMAND%% *} isn't installed"
fi

if [ -n "$GPU_MONITOR_COMMAND" ]; then
    shimmer_has "${GPU_MONITOR_COMMAND%% *}" ||
        shimmer_fail "GPU_MONITOR_COMMAND's ${GPU_MONITOR_COMMAND%% *} isn't installed"
fi

if [ -n "$BROWSER_COMMAND" ]; then
    shimmer_has "${BROWSER_COMMAND%% *}" || shimmer_fail "BROWSER_COMMAND's ${BROWSER_COMMAND%% *} isn't installed"
fi

# Every *_PLACE answer: a valid "WORKSPACE:KEY" shape, and its monitor question filled in. A
# typo here should stop the launch, not silently open unplaced.
for pair in "STEAM_PLACE:$STEAM_PLACE" "GAME_PLACE:$GAME_PLACE" "VM_PLACE:$VM_PLACE" \
    "MONITORS_PLACE:$MONITORS_PLACE" "BROWSER_PLACE:$BROWSER_PLACE"; do
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

if [ -n "$GAME_LIBRARY" ]; then
    game_summary=", game: picker over $(printf '%s' "$GAME_LIBRARY" | tr '|' '\n' | wc -l) titles"
else
    game_summary="${GAME_COMMAND:+, game: $GAME_COMMAND}"
fi
echo "ok${STEAM_COMMAND:+, steam: $STEAM_COMMAND}${game_summary}${VM_NAME:+, vm: $VM_NAME}${SYSTEM_MONITOR_COMMAND:+, monitor: $SYSTEM_MONITOR_COMMAND}${GPU_MONITOR_COMMAND:+, gpu: $GPU_MONITOR_COMMAND}${BROWSER_COMMAND:+, browser: $BROWSER_COMMAND}"
