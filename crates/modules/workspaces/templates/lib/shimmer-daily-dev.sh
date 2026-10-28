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

# Some apps (notably Electron apps like Discord, which fork or re-exec into a different
# process on startup) lose Hyprland's one-shot `[workspace N]` exec rule, since Hyprland tracks
# it by the PID the dispatch call itself spawned -- a fork/re-exec can break that link, and the
# window then opens wherever is currently focused instead. Poll (up to ~8s, since Electron apps
# are often slow to actually map a window) for a window of CLASS whose address wasn't in BEFORE
# -- genuinely new since this call's own dispatch, not just any window of that class, since
# several launches can share one class (every kitty-wrapped step reports "kitty"). Moves it to
# WORKSPACE if it landed elsewhere -- still a one-shot dispatch, never a persistent window rule.
# Prints the new window's address on success (so the caller can remember exactly which window
# is "this launch", not just its class) and nothing if none appeared in time.
shimmer_hyprland_ensure_workspace() {
    ws=$1
    class=$2
    label=$3
    before=$4
    i=0
    while [ "$i" -lt 16 ]; do
        # CLASS is only a guess (the launching command's own first word), and several real
        # classes don't match their command at all (zen-browser's window reports "zen",
        # Obsidian's reports "md.obsidian.Obsidian") -- matched either direction as a
        # substring, not exact equality, so the guess still finds the real window.
        found=$(hyprctl clients -j 2>/dev/null | jq -r --arg class "$class" --arg before "$before" '
            ($before | split("\n")) as $old |
            ($class | ascii_downcase) as $want |
            ([.[] | (.class // "" | ascii_downcase) as $got |
                select($got != "" and ($got | contains($want) or ($want | contains($got)))) |
                select((.address as $a | $old | index($a)) == null)][0]) as $w |
            if $w == null then "" else "\($w.workspace.id) \($w.address)" end' 2>/dev/null)
        if [ -n "$found" ]; then
            # shellcheck disable=SC2086
            set -- $found
            actual_ws=$1
            addr=$2
            if [ "$actual_ws" != "$ws" ]; then
                hyprctl dispatch movetoworkspacesilent "$ws,address:$addr" >/dev/null 2>&1
                shimmer_daily_dev_log "$label: Hyprland's exec rule missed (landed on workspace $actual_ws), moved to $ws instead"
            fi
            printf '%s' "$addr"
            return 0
        fi
        sleep 0.5
        i=$((i + 1))
    done
    shimmer_daily_dev_log "$label: no new window matching class '$class' appeared within 8s, gave up correcting its workspace"
    return 1
}

# Is WORKSPACE currently shown on MONITOR? The "monitor" clause in the original dispatch exec
# rule can be bypassed when Hyprland has to create the workspace fresh, landing it on whatever
# monitor was last active instead of the one asked for. Move the whole workspace there if not
# -- still a one-shot dispatch, never a persistent monitor rule.
shimmer_hyprland_ensure_monitor() {
    ws=$1
    mon=$2
    label=$3
    current=$(hyprctl monitors -j 2>/dev/null | jq -r --argjson ws "$ws" '[.[] | select(.activeWorkspace.id == $ws)][0].name // ""')
    [ -n "$current" ] && [ "$current" != "$mon" ] || return 0
    hyprctl dispatch moveworkspacetomonitor "$ws" "$mon" >/dev/null 2>&1
    shimmer_daily_dev_log "$label: workspace $ws was showing on $current instead of $mon, moved"
}

# Is the window at ADDRESS still open? Checking Hyprland's live state, not this script's own
# process group, since a placed window is spawned by Hyprland itself.
shimmer_hyprland_window_alive() {
    [ -n "$1" ] && hyprctl clients -j 2>/dev/null | jq -e --arg addr "$1" 'any(.[]; .address == $addr)' >/dev/null 2>&1
}

# A detached step is spawned and left running by the daemon, which doesn't wait for one
# step's script to finish before starting the next (ADR 0010 S2a) -- so editor,
# system-monitor and gpu-monitor's scripts can genuinely run at the same time, all three
# reporting class "kitty". Without this, their before/after snapshots can interleave: one
# step's "new window" poll can catch a sibling's window instead of its own. Serialize every
# CLASS-matching launch against each other with a plain lock file, so only one of them is
# between its own dispatch and identifying its own window at a time. Gives up (proceeds
# anyway, same as a timeout elsewhere here) after 10s rather than waiting forever on a stale
# lock from a crashed step.
shimmer_hyprland_lock() {
    lock="$SHIMMER_STATE_DIR/hyprland-lock-$1"
    mkdir -p "$SHIMMER_STATE_DIR"
    i=0
    while [ "$i" -lt 40 ]; do
        (set -C; : >"$lock") 2>/dev/null && return 0
        sleep 0.25
        i=$((i + 1))
    done
    return 0
}

shimmer_hyprland_unlock() {
    rm -f "$SHIMMER_STATE_DIR/hyprland-lock-$1"
}

# Run COMMAND (already a full command line, e.g. "kitty btop" or "obsidian '/my/vault'"),
# placed on PLACE's workspace and monitor if PLACE parses and both Hyprland and that monitor
# are available right now; otherwise COMMAND runs as an ordinary background launch, with one
# `note:` line saying why placement didn't happen. Either way LABEL is recorded as left open: a
# window Hyprland itself spawns (not this script) is one `workspaces stop` can never find to
# close, since it never goes through this script's own process group.
#
# SLOT names this launch for its own marker file (e.g. "editor", "chat") -- short and
# filename-safe, not the human-readable LABEL. Whether this launch is "already open from the
# last activate" is never guessed from class or workspace alone: several launches share one
# class (every kitty-wrapped step reports "kitty"), and a workspace can already hold a window
# that has nothing to do with this app at all (another terminal, a VM manager window) -- either
# one looks exactly like "already placed" to a check that isn't keyed on the specific window
# this exact SLOT created last time. The marker remembers that window's address; only that
# address still being alive counts as "already open".
shimmer_hyprland_launch() {
    place=$1
    command=$2
    label=$3
    slot=$4
    marker="$SHIMMER_STATE_DIR/hyprland-placed-$slot"
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
    if [ -f "$marker" ] && shimmer_hyprland_window_alive "$(cat "$marker" 2>/dev/null)"; then
        msg="already open from the last activate: $label"
        echo "$msg"
        shimmer_daily_dev_log "$msg"
        shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
        return 0
    fi
    rm -f "$marker"
    class=${command%% *}
    class=${class##*/}
    shimmer_hyprland_lock "$class"
    before=$(hyprctl clients -j 2>/dev/null | jq -r '.[].address' 2>/dev/null)
    shimmer_daily_dev_log "$label: placing on workspace $ws ($key monitor = $mon), class guess '$class': $command"
    hyprctl dispatch exec "[workspace $ws; monitor $mon] $command" >/dev/null 2>&1
    shimmer_record_left_open "$label (placed: workspace $ws, $key monitor)"
    addr=$(shimmer_hyprland_ensure_workspace "$ws" "$class" "$label" "$before")
    shimmer_hyprland_ensure_monitor "$ws" "$mon" "$label"
    shimmer_hyprland_unlock "$class"
    if [ -n "$addr" ]; then
        mkdir -p "$SHIMMER_STATE_DIR"
        printf '%s' "$addr" >"$marker"
    fi
}
