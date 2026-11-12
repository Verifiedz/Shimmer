# Detached: a terminal on this computer, logged into the VM over SSH, landing in REMOTE_FOLDER
# when it's set (shimmer_ssh_command), the VM's home folder otherwise. With no key and no
# keychain entry, ssh asks for the password in that window: Shimmer never sees it. Placed by
# TERMINAL_PLACE, but only when TERMINAL_APP resolves to kitty -- the same restriction the
# daily-dev/gaming templates' own monitor steps use, since placement needs to build the exact
# command a terminal launches rather than letting shimmer_open_terminal pick per-terminal.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ "${TERMINAL_APP:-none}" = "none" ] && exit 0
host=$(shimmer_vm_host)
[ -n "$host" ] || exit 0
ssh_cmd=$(shimmer_ssh_command "$(shimmer_ssh_target "$host")")

term=${TERMINAL_APP:-auto}
[ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
if [ -n "$TERMINAL_PLACE" ] && [ "$term" = "kitty" ]; then
    shell=${SHELL:-sh}
    inner="cd $(shimmer_quote "$HOME") && $ssh_cmd; exec $(shimmer_quote "$shell")"
    command="kitty --directory $(shimmer_quote "$HOME") $(shimmer_quote "$shell") -c $(shimmer_quote "$inner")"
    shimmer_hyprland_launch "$TERMINAL_PLACE" "$command" "the SSH terminal into the VM" terminal
else
    if [ -n "$TERMINAL_PLACE" ]; then
        msg="TERMINAL_PLACE is set, but TERMINAL_APP resolves to '$term', not kitty: opening the terminal without placement"
        echo "note: $msg"
        shimmer_hyprland_log "$msg"
    fi
    shimmer_open_terminal "$TERMINAL_APP" "$HOME" "$ssh_cmd"
fi
