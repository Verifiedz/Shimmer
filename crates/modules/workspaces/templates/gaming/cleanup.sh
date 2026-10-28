# Cleanup (supervised): close every Hyprland-placed window by its exact remembered address,
# where CLOSE_ON_STOP allows it, and list what it left open. An app you left unplaced is
# always left open: with no placement there's no address to target.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

shimmer_close_windows
shimmer_hyprland_close_all
