# Detached: the Obsidian vault (OBSIDIAN_VAULT).
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-daily-dev.sh"

[ -n "$OBSIDIAN_VAULT" ] || exit 0
shimmer_hyprland_launch "$NOTES_PLACE" "obsidian $(shimmer_quote "$OBSIDIAN_VAULT")" \
    "the Obsidian window (Obsidian keeps every vault in one program, and may hold unsaved edits)" notes
