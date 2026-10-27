# Supervised: stop with one clear sentence before anything opens.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

problem=$(shimmer_display_problem)
[ -z "$problem" ] || shimmer_fail "$problem"

if [ -n "$PROJECT_DIR" ] || [ -n "$SYSTEM_MONITOR_COMMAND" ] || [ -n "$GPU_MONITOR_COMMAND" ]; then
    shimmer_has_terminal "${TERMINAL_APP:-none}" || shimmer_fail "the terminal '$TERMINAL_APP' isn't installed here"
    [ "${TERMINAL_APP:-none}" != "none" ] ||
        shimmer_fail "TERMINAL_APP is none, but a project folder or a monitor command is set: set TERMINAL_APP to auto or a terminal"
fi

if [ -n "$PROJECT_DIR" ]; then
    [ -d "$PROJECT_DIR" ] || shimmer_fail "the project folder $PROJECT_DIR doesn't exist"
    shimmer_has nvim || shimmer_fail "neovim (nvim) isn't installed"
fi

if [ -n "$OBSIDIAN_VAULT" ]; then
    [ -d "$OBSIDIAN_VAULT" ] || shimmer_fail "the Obsidian vault $OBSIDIAN_VAULT doesn't exist"
    shimmer_has obsidian || shimmer_fail "obsidian isn't installed (or isn't on PATH)"
fi

if [ -n "$SYSTEM_MONITOR_COMMAND" ]; then
    shimmer_has "${SYSTEM_MONITOR_COMMAND%% *}" ||
        shimmer_fail "SYSTEM_MONITOR_COMMAND's ${SYSTEM_MONITOR_COMMAND%% *} isn't installed"
fi

if [ -n "$GPU_MONITOR_COMMAND" ]; then
    shimmer_has "${GPU_MONITOR_COMMAND%% *}" ||
        shimmer_fail "GPU_MONITOR_COMMAND's ${GPU_MONITOR_COMMAND%% *} isn't installed"
fi

if [ -n "$DISCORD_COMMAND" ]; then
    shimmer_has "${DISCORD_COMMAND%% *}" || shimmer_fail "DISCORD_COMMAND's ${DISCORD_COMMAND%% *} isn't installed"
fi

if [ -n "$BROWSER_COMMAND" ]; then
    shimmer_has "${BROWSER_COMMAND%% *}" || shimmer_fail "BROWSER_COMMAND's ${BROWSER_COMMAND%% *} isn't installed"
fi

# Every *_PLACE answer: a valid "WORKSPACE:KEY" shape, and its monitor question filled in. A
# typo here should stop the launch, not silently open unplaced.
for pair in "EDITOR_PLACE:$EDITOR_PLACE" "MONITORS_PLACE:$MONITORS_PLACE" "NOTES_PLACE:$NOTES_PLACE" \
    "CHAT_PLACE:$CHAT_PLACE" "BROWSER_PLACE:$BROWSER_PLACE"; do
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

echo "ok${PROJECT_DIR:+, project: $PROJECT_DIR}${OBSIDIAN_VAULT:+, vault: $OBSIDIAN_VAULT}${SYSTEM_MONITOR_COMMAND:+, monitor: $SYSTEM_MONITOR_COMMAND}${GPU_MONITOR_COMMAND:+, gpu: $GPU_MONITOR_COMMAND}${DISCORD_COMMAND:+, chat: $DISCORD_COMMAND}${BROWSER_COMMAND:+, browser: $BROWSER_COMMAND}"
