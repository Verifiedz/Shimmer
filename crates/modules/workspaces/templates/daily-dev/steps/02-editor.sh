# Detached: Neovim on the project folder, in a terminal (TERMINAL_APP).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$PROJECT_DIR" ] || exit 0

term=${TERMINAL_APP:-auto}
[ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
if [ -n "$EDITOR_PLACE" ] && [ "$term" = "kitty" ]; then
    shimmer_hyprland_launch "$EDITOR_PLACE" "kitty --directory $(shimmer_quote "$PROJECT_DIR") nvim" "the Neovim window" editor
else
    if [ -n "$EDITOR_PLACE" ]; then
        msg="EDITOR_PLACE is set, but TERMINAL_APP resolves to '$term', not kitty: opening Neovim without placement"
        echo "note: $msg"
        shimmer_daily_dev_log "$msg"
    fi
    shimmer_open_editor neovim "$PROJECT_DIR"
fi
