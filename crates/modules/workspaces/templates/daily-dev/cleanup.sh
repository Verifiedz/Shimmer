# Cleanup (supervised): close the terminals it opened for Neovim and the monitors where
# CLOSE_ON_STOP allows it, and close every Hyprland-placed window by its exact remembered
# address, also where CLOSE_ON_STOP allows it. An unplaced Obsidian, chat app or browser is
# always left open regardless: with no placement there's no address to target, and closing by
# class or title alone could just as easily hit a window that has nothing to do with this
# workspace.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

shimmer_close_windows
shimmer_hyprland_close_all
