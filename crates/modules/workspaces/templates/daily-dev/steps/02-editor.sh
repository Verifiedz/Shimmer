# Detached: Neovim on the project folder, in a terminal (TERMINAL_APP).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

[ -n "$PROJECT_DIR" ] || exit 0

term=${TERMINAL_APP:-auto}
[ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
if [ -n "$EDITOR_PLACE" ] && [ "$term" = "kitty" ]; then
    shimmer_hyprland_launch "$EDITOR_PLACE" "kitty --directory $(shimmer_quote "$PROJECT_DIR") nvim" "the Neovim window"
else
    [ -n "$EDITOR_PLACE" ] && echo "note: EDITOR_PLACE is set, but TERMINAL_APP resolves to '$term', not kitty: opening Neovim without placement"
    shimmer_open_editor neovim "$PROJECT_DIR"
fi
