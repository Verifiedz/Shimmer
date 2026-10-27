# Cleanup (supervised): close the terminals it opened for Neovim and the monitors where
# CLOSE_ON_STOP allows it and they weren't placed with Hyprland, and list what it left open.
# Obsidian, your chat app, your browser, and anything placed with Hyprland are always left
# open: Shimmer can't tell one of their windows from any other they already have.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"

shimmer_close_windows
