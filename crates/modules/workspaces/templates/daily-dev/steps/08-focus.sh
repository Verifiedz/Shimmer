# Supervised: focus back to Neovim's workspace, if EDITOR_PLACE is set and Hyprland is ready.
# A small nicety, same as a hand-written Hyprland script ending on `hyprctl dispatch workspace`.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

if [ -n "$EDITOR_PLACE" ] && shimmer_hyprland_ready; then
    hyprctl dispatch workspace "${EDITOR_PLACE%%:*}" >/dev/null 2>&1
fi
exit 0
