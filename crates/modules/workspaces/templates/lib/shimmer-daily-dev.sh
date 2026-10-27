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

# For the check step: does VALUE (a *_COMMAND question's answer) look like a *_PLACE value
# ("4:second") that landed in the wrong question? Both kinds of question mention "monitor" --
# the app (SYSTEM_MONITOR_COMMAND) and the physical display (MONITORS_PLACE) -- and they're
# only a couple of questions apart in the wizard, so this is a plausible mix-up worth a
# specific message rather than the much less helpful "isn't installed" the command-existence
# check below would otherwise give.
shimmer_warn_if_place() {
    name=$1
    value=$2
    place_name=$3
    [ -n "$value" ] || return 0
    if shimmer_hyprland_place_ok "$value"; then
        shimmer_fail "$name is '$value', which looks like a workspace:monitor placement value, not a command: did you mean $place_name instead?"
    fi
    return 0
}

# Append MESSAGE to a real log file. Detached steps (editor, the monitors, notes, chat,
# browser, focus) have their stdout thrown away by the daemon -- only a supervised step (check)
# gets one (ADR 0010 S2a) -- so a plain `echo` from any of them is never seen anywhere. This is
# the only way to leave a trail of what placement actually did: tail -f
# "$SHIMMER_HOME/logs/$SHIMMER_WORKSPACE_ID-daily-dev.log" during activate to watch it live.
shimmer_daily_dev_log() {
    dir="$SHIMMER_HOME/logs"
    mkdir -p "$dir" 2>/dev/null || return 0
    printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" >>"$dir/$SHIMMER_WORKSPACE_ID-daily-dev.log" 2>/dev/null
}

# A snapshot of every client Hyprland currently has, taken once at the start of this activation
# (the check step calls this). Steps check against the snapshot, not live state: MONITORS_PLACE
# puts both the system and GPU monitor on the same workspace in the same activation, and if the
# GPU monitor's step asked live after the system monitor's step had already opened its window,
# it would see that workspace "already occupied" and skip itself too. A snapshot taken before
# either ran doesn't have that problem, while still correctly catching a genuine repeat
# `workspaces activate` (where the previous activation's windows are already there before this
# one starts).
shimmer_hyprland_snapshot() {
    shimmer_hyprland_ready || return 0
    mkdir -p "$SHIMMER_STATE_DIR"
    hyprctl clients -j 2>/dev/null >"$SHIMMER_STATE_DIR/hyprland-clients.json" || :
}

# Does the snapshot already have a window of CLASS on WORKSPACE? A placed launch has no
# process group Shimmer can check (unlike shimmer_window_open, which the plain, unplaced path
# uses) -- Hyprland itself spawned it -- so this is the placed path's own "don't reopen what's
# already there". Matched by class, not just "is the workspace non-empty": that workspace may
# already hold something this app had nothing to do with (another terminal, a VM manager
# window), which looked exactly like "already placed" and silently skipped the real launch.
# CLASS is a best-effort guess (the launching command's own first word); a real class that
# doesn't match it just means a genuine repeat activation opens another window instead of
# reusing the old one -- the same piling-up this exists to prevent, not a worse failure than
# before this existed.
shimmer_hyprland_workspace_occupied() {
    ws=$1
    class=$2
    [ -f "$SHIMMER_STATE_DIR/hyprland-clients.json" ] || return 1
    jq -e --argjson ws "$ws" --arg class "$class" \
        'any(.[]; .workspace.id == $ws and (.class // "" | ascii_downcase) == ($class | ascii_downcase))' \
        "$SHIMMER_STATE_DIR/hyprland-clients.json" >/dev/null 2>&1
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
        shimmer_daily_dev_log "$label: no placement set, opening plainly ($command)"
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    if ! shimmer_hyprland_ready; then
        msg="$label has a placement set ($place), but Hyprland, hyprctl or jq isn't available here: opening it without placement"
        echo "note: $msg"
        shimmer_daily_dev_log "$msg"
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    ws=${place%%:*}
    key=${place#*:}
    mon=$(shimmer_hyprland_monitor_name "$key")
    if [ -z "$mon" ]; then
        msg="$label's $key monitor isn't connected right now: opening it without placement"
        echo "note: $msg"
        shimmer_daily_dev_log "$msg"
        sh -c "$command" >/dev/null 2>&1 &
        shimmer_record_left_open "$label"
        return 0
    fi
    class=${command%% *}
    class=${class##*/}
    if shimmer_hyprland_workspace_occupied "$ws" "$class"; then
        msg="already open from the last activate: $label (workspace $ws, class $class)"
        echo "$msg"
        shimmer_daily_dev_log "$msg"
        shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
        return 0
    fi
    shimmer_daily_dev_log "$label: placing on workspace $ws ($key monitor = $mon), class guess '$class': $command"
    hyprctl dispatch exec "[workspace $ws; monitor $mon] $command" >/dev/null 2>&1
    shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
}
