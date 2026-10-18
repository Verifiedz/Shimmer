# Helpers for the health template (ADR 0025): each check of the report. Steps load it after
# shimmer-open.sh:
#   . "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
#   . "$SHIMMER_WORKSPACE_DIR/lib/shimmer-health.sh"
#
# Read-only: nothing here changes anything or needs sudo. Each check prints lines of
# "ok<TAB>text", "warn<TAB>text" (needs attention) or "info<TAB>text", and prints nothing when it
# doesn't apply here (no battery, no systemd…).
#
# POSIX sh only. This is your copy: Shimmer never changes it after the workspace is created, so
# add a check of your own here if you like (and its name to SHIMMER_HEALTH_CHECKS).

SHIMMER_HEALTH_CHECKS="disk memory load uptime battery temperature services workspaces updates ports docker folders"
SHIMMER_RESULTS="$SHIMMER_STATE_DIR/results"

shimmer_health_line() {
    printf '%s\t%s\n' "$1" "$2"
}

# 1536 → "1.5 MB", for kilobytes.
shimmer_health_human() {
    awk -v kb="${1:-0}" 'BEGIN {
        if (kb >= 1048576) printf "%.1f GB", kb / 1048576
        else if (kb >= 1024) printf "%.0f MB", kb / 1024
        else printf "%d KB", kb
    }'
}

# Every real disk: one line per device (btrfs and APFS list one device many times), skipping
# loop devices (snaps) and macOS's own system volumes.
health_disk() {
    df -Pk 2>/dev/null | awk -v warn="${DISK_WARN_PERCENT:-90}" '
        NR == 1 { next }
        $1 !~ /^\/dev\// || $1 ~ /^\/dev\/loop/ { next }
        $6 ~ /^\/System\/Volumes\// && $6 != "/System/Volumes/Data" { next }
        seen[$1]++ { next }
        {
            pct = $5; sub(/%/, "", pct)
            free = $4 / 1048576
            state = (pct + 0 >= warn) ? "warn" : "ok"
            printf "%s\tdisk %s: %d%% used, %.1f GB free\n", state, $6, pct, free
        }'
}

health_memory() {
    if [ -r /proc/meminfo ]; then
        awk -v warn="${MEMORY_WARN_PERCENT:-90}" '
            /^MemTotal:/ { total = $2 } /^MemAvailable:/ { avail = $2 }
            /^SwapTotal:/ { stotal = $2 } /^SwapFree:/ { sfree = $2 }
            END {
                if (!total) exit
                pct = (total - avail) * 100 / total
                state = (pct >= warn) ? "warn" : "ok"
                printf "%s\tmemory: %d%% used, %.1f GB of %.1f GB free", state, pct, avail / 1048576, total / 1048576
                if (stotal > 0) printf ", swap %d%% used", (stotal - sfree) * 100 / stotal
                printf "\n"
            }' /proc/meminfo
    elif shimmer_macos; then
        total=$(sysctl -n hw.memsize 2>/dev/null)
        [ -n "$total" ] || return 0
        vm_stat 2>/dev/null | awk -v total="$total" -v warn="${MEMORY_WARN_PERCENT:-90}" '
            /page size of/ { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+$/) size = $i }
            /^Pages (free|inactive|speculative|purgeable):/ { gsub(/\./, "", $NF); free += $NF }
            END {
                avail = free * size; pct = (total - avail) * 100 / total
                state = (pct >= warn) ? "warn" : "ok"
                printf "%s\tmemory: %d%% used, %.1f GB of %.1f GB free\n", state, pct, avail / 1073741824, total / 1073741824
            }'
    fi
}

health_load() {
    cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
    if [ -r /proc/loadavg ]; then
        load=$(awk '{ print $3 }' /proc/loadavg)
    elif shimmer_macos; then
        load=$(sysctl -n vm.loadavg 2>/dev/null | awk '{ print $4 }')
    fi
    [ -n "$load" ] || return 0
    awk -v load="$load" -v cpus="$cpus" 'BEGIN {
        state = (load > cpus) ? "warn" : "ok"
        printf "%s\tcpu: load %.2f over 15 minutes on %d cores%s\n", state, load, cpus, (load > cpus) ? " (busier than it can keep up with)" : ""
    }'
}

health_uptime() {
    if [ -r /proc/uptime ]; then
        seconds=$(awk '{ printf "%d", $1 }' /proc/uptime)
    elif shimmer_macos; then
        boot=$(sysctl -n kern.boottime 2>/dev/null | sed 's/.*sec = \([0-9]*\).*/\1/')
        [ -n "$boot" ] && seconds=$(($(date +%s) - boot))
    fi
    [ -n "$seconds" ] || return 0
    days=$((seconds / 86400))
    unit=days
    [ "$days" -eq 1 ] && unit=day
    if [ -f /var/run/reboot-required ]; then
        shimmer_health_line warn "restart needed: updates installed since boot ask for one (up $days $unit)"
    elif [ "$(uname -s)" = "Linux" ] && [ -d /usr/lib/modules ] && [ ! -d "/usr/lib/modules/$(uname -r)" ]; then
        shimmer_health_line warn "restart needed: the kernel was updated since boot (up $days $unit)"
    elif [ "$days" -ge "${UPTIME_WARN_DAYS:-30}" ]; then
        shimmer_health_line warn "up $days $unit: a restart picks up updates and clears leaks"
    else
        shimmer_health_line ok "up $days $unit"
    fi
}

health_battery() {
    if shimmer_macos; then
        info=$(system_profiler SPPowerDataType 2>/dev/null)
        printf '%s\n' "$info" | grep -q "Cycle Count" || return 0
        cycles=$(printf '%s\n' "$info" | awk -F': ' '/Cycle Count/ { print $2; exit }')
        health=$(printf '%s\n' "$info" | awk -F': ' '/Maximum Capacity/ { sub(/%/, "", $2); print $2; exit }')
        charge=$(pmset -g batt 2>/dev/null | grep -o '[0-9]*%' | head -n 1)
        [ -n "$health" ] || return 0
        state=ok
        [ "$health" -lt "${BATTERY_WARN_PERCENT:-80}" ] && state=warn
        shimmer_health_line "$state" "battery: health $health% of new, $cycles cycles, charged ${charge:-?}"
        return 0
    fi
    for bat in /sys/class/power_supply/BAT*; do
        [ -d "$bat" ] || continue
        full=$(cat "$bat/energy_full" 2>/dev/null || cat "$bat/charge_full" 2>/dev/null)
        design=$(cat "$bat/energy_full_design" 2>/dev/null || cat "$bat/charge_full_design" 2>/dev/null)
        charge=$(cat "$bat/capacity" 2>/dev/null)
        cycles=$(cat "$bat/cycle_count" 2>/dev/null)
        status=$(cat "$bat/status" 2>/dev/null)
        if [ -n "$full" ] && [ -n "$design" ] && [ "$design" -gt 0 ]; then
            health=$((full * 100 / design))
            state=ok
            [ "$health" -lt "${BATTERY_WARN_PERCENT:-80}" ] && state=warn
            shimmer_health_line "$state" "battery $(basename "$bat"): health $health% of new${cycles:+, $cycles cycles}, ${charge:-?}% ${status:+($status)}"
        elif [ -n "$charge" ]; then
            shimmer_health_line info "battery $(basename "$bat"): ${charge}% ${status:+($status)}"
        fi
    done
}

# The hottest sensor (Linux; macOS needs sudo for this, so it's left out there).
health_temperature() {
    max=0
    for zone in /sys/class/thermal/thermal_zone*/temp; do
        [ -r "$zone" ] || continue
        t=$(cat "$zone" 2>/dev/null)
        [ -n "$t" ] && [ "$t" -gt "$max" ] && max=$t
    done
    [ "$max" -gt 0 ] || return 0
    c=$((max / 1000))
    if [ "$c" -ge 85 ]; then
        shimmer_health_line warn "temperature: hottest sensor at ${c}°C"
    else
        shimmer_health_line ok "temperature: hottest sensor at ${c}°C"
    fi
}

health_services() {
    shimmer_has systemctl || return 0
    system=$(systemctl --failed --no-legend --plain 2>/dev/null | awk '{ print $1 }' | tr '\n' ' ')
    user=$(systemctl --user --failed --no-legend --plain 2>/dev/null | awk '{ print $1 }' | tr '\n' ' ')
    if [ -n "$system$user" ]; then
        shimmer_health_line warn "failed services: $system${user:+(yours: $user)}— see: systemctl status <name>"
    else
        shimmer_health_line ok "services: none failed"
    fi
}

# Shimmer's own workspaces, through the daemon this step was started by.
health_workspaces() {
    shimmer_has shimmer || return 0
    bad=$(shimmer workspaces list 2>/dev/null | awk 'NR > 1 && ($2 == "dirty" || $2 == "invalid") { printf "%s%s (%s)", sep, $1, $2; sep = ", " }')
    if [ -n "$bad" ]; then
        shimmer_health_line warn "workspaces needing a fix: $bad — see: shimmer workspaces show <id>"
    else
        shimmer_health_line ok "workspaces: none dirty"
    fi
}

# Updates waiting, without sudo and without installing anything.
health_updates() {
    count=""
    if shimmer_has checkupdates; then
        count=$(checkupdates 2>/dev/null | grep -c .)
        what="system packages"
    elif shimmer_has apt; then
        count=$(apt list --upgradable 2>/dev/null | grep -c /)
        what="system packages (as of the last apt update)"
    elif shimmer_has brew; then
        count=$(brew outdated --quiet 2>/dev/null | grep -c .)
        what="brew packages"
    fi
    [ -n "$count" ] || return 0
    if [ "$count" -gt 0 ]; then
        shimmer_health_line info "$count $what waiting to update"
    else
        shimmer_health_line ok "$what: up to date"
    fi
}

# What of yours is listening on a port: forgotten dev servers show up here. Named by the
# program that was started (a thread's name, like "MainThread", says nothing).
health_ports() {
    if shimmer_has ss; then
        pairs=$(ss -ltnpH 2>/dev/null | awk '/pid=/ {
            port = $4; sub(/.*:/, "", port)
            pid = $0; sub(/.*pid=/, "", pid); sub(/[^0-9].*/, "", pid)
            print pid, port }')
    elif shimmer_has lsof; then
        pairs=$(lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 { port = $9; sub(/.*:/, "", port); print $2, port }')
    else
        return 0
    fi
    list=$(printf '%s\n' "$pairs" | while read -r pid port; do
        [ -n "$pid" ] || continue
        program=$(ps -o args= -p "$pid" 2>/dev/null | awk '{ print $1 }')
        echo "$(basename "${program:-pid $pid}") :$port"
    done | sort -u | sort -t: -k2,2n | tr '\n' ',' | sed 's/,$//; s/,/, /g')
    if [ -n "$list" ]; then
        shimmer_health_line info "listening (yours): $list"
    else
        shimmer_health_line ok "ports: nothing of yours listening"
    fi
}

health_docker() {
    shimmer_has docker || return 0
    docker info >/dev/null 2>&1 || return 0
    # Only the kinds with something to free: "Images: 2.1GB (40%)".
    reclaim=$(docker system df --format '{{.Type}}: {{.Reclaimable}}' 2>/dev/null | grep -v ': 0B' | tr '\n' ',' | sed 's/,$//; s/,/, /g')
    if [ -n "$reclaim" ]; then
        shimmer_health_line info "docker could free: $reclaim (a free-disk workspace cleans it)"
    else
        shimmer_health_line ok "docker: nothing to free"
    fi
}

# The biggest folders and files directly in your home (BIGGEST_FOLDERS of them).
health_folders() {
    n=${BIGGEST_FOLDERS:-5}
    [ "$n" -gt 0 ] 2>/dev/null || return 0
    list=$(du -xsk "$HOME"/* "$HOME"/.[!.]* 2>/dev/null | sort -rn | head -n "$n" | while read -r kb path; do
        printf '%s %s, ' "$(shimmer_health_human "$kb")" "~${path#"$HOME"}"
    done | sed 's/, $//')
    [ -n "$list" ] && shimmer_health_line info "biggest in your home: $list"
}
