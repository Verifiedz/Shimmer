# Shared helpers for the daily-dev template's optional Hyprland placement: putting an app on a
# specific workspace and monitor at launch time with `hyprctl dispatch exec`, the same technique
# a hand-written Hyprland script already uses. Never reads or writes the Hyprland config: it's
# all one-shot dispatch rules at exec time.
#
# A *_PLACE value is "WORKSPACE:KEY", e.g. "1:main" or "4:second". KEY is "main" or "second",
# resolved against HYPRLAND_MAIN_MONITOR / HYPRLAND_SECOND_MONITOR (a substring of that
# monitor's hardware description, as `hyprctl monitors -j` reports it).

# Is Hyprland running here, with hyprctl and jq available?
shimmer_hyprland_ready() {
    [ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] && shimmer_has hyprctl && shimmer_has jq
}

# The real output name (e.g. DP-2) for KEY (main/second), or nothing if it can't be resolved
# right now (the matching HYPRLAND_*_MONITOR question is empty, or that monitor isn't
# connected).
shimmer_hyprland_monitor_name() {
    key=$1
    case "$key" in
        main) desc=$HYPRLAND_MAIN_MONITOR ;;
        second) desc=$HYPRLAND_SECOND_MONITOR ;;
        *) return 0 ;;
    esac
    [ -n "$desc" ] || return 0
    hyprctl monitors -j 2>/dev/null | jq -r --arg d "$desc" '[.[] | select(.description | contains($d))][0].name // ""'
}

# Does PLACE look like "WORKSPACE:KEY" (one or more digits, a colon, then main or second)? For
# the check step: a typo here should stop the launch, not silently open unplaced.
shimmer_hyprland_place_ok() {
    case "$1" in
        [0-9]*:main | [0-9]*:second) ;;
        *) return 1 ;;
    esac
    case "${1%%:*}" in
        *[!0-9]*) return 1 ;;
    esac
    return 0
}

# A snapshot of occupied workspace ids, taken once at the start of this activation (the check
# step calls this). Steps check against the snapshot, not live state: MONITORS_PLACE puts both
# the system and GPU monitor on the same workspace in the same activation, and if the GPU
# monitor's step asked live after the system monitor's step had already opened its window, it
# would see that workspace "already occupied" and skip itself too. A snapshot taken before
# either ran doesn't have that problem, while still correctly catching a genuine repeat
# `workspaces activate` (where the previous activation's windows are already there before this
# one starts).
shimmer_hyprland_snapshot() {
    shimmer_hyprland_ready || return 0
    mkdir -p "$SHIMMER_STATE_DIR"
    hyprctl clients -j 2>/dev/null | jq -r '[.[].workspace.id] | unique | .[]' >"$SHIMMER_STATE_DIR/hyprland-occupied" 2>/dev/null
}

# Was WORKSPACE occupied in that snapshot? A placed launch has no process group Shimmer can
# check (unlike shimmer_window_open, which the plain, unplaced path uses) -- Hyprland itself
# spawned it -- so this is the placed path's own "don't reopen what's already there": running
# `workspaces activate` again on an already-active workspace would otherwise pile up a new
# window on every call, instead of leaving the existing one alone like every other step here
# does.
shimmer_hyprland_workspace_occupied() {
    [ -f "$SHIMMER_STATE_DIR/hyprland-occupied" ] && grep -qx "$1" "$SHIMMER_STATE_DIR/hyprland-occupied" 2>/dev/null
}

# Run COMMAND (already a full command line, e.g. "kitty btop" or "obsidian '/my/vault'"),
# placed on PLACE's workspace and monitor if PLACE parses and both Hyprland and that monitor
# are available right now; otherwise COMMAND runs as an ordinary background launch, with one
# `note:` line saying why placement didn't happen. Either way LABEL is recorded as left open: a
# window Hyprland itself spawns (not this script) is one `workspaces stop` can never find to
# close, since it never goes through this script's own process group.
shimmer_hyprland_launch() {
    place=$1
    command=$2
    label=$3
    if [ -z "$place" ]; then
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    if ! shimmer_hyprland_ready; then
        echo "note: $label has a placement set ($place), but Hyprland, hyprctl or jq isn't available here: opening it without placement"
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    ws=${place%%:*}
    key=${place#*:}
    mon=$(shimmer_hyprland_monitor_name "$key")
    if [ -z "$mon" ]; then
        echo "note: $label's $key monitor isn't connected right now: opening it without placement"
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    if shimmer_hyprland_workspace_occupied "$ws"; then
        echo "already open from the last activate: $label (workspace $ws)"
        shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
        return 0
    fi
    hyprctl dispatch exec "[workspace $ws; monitor $mon] $command" >/dev/null 2>&1
    shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
}
