# Detached: open a folder inside the VM in Cursor or VS Code, over SSH (Remote-SSH). The editor
# asks for a password itself if there's no SSH key. Placed by EDITOR_PLACE if set -- even though
# the editor normally shares one program with your other windows, Hyprland still tracks this
# exact window's own address, so it can be found and closed later despite that.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ "${REMOTE_EDITOR:-none}" = "none" ] && exit 0
host=$(shimmer_vm_host)
[ -n "$host" ] || exit 0
target=$(shimmer_ssh_target "$host")
# Remote-SSH takes the port from ~/.ssh/config, not from the address.
[ "${SSH_PORT:-22}" = "22" ] || echo "port $SSH_PORT: add it to a Host entry in ~/.ssh/config for the editor"
cli=$(shimmer_remote_editor_cli "$REMOTE_EDITOR")
if [ -n "$REMOTE_FOLDER" ]; then
    command="$cli --remote $(shimmer_quote "ssh-remote+$target") $(shimmer_quote "$REMOTE_FOLDER")"
else
    command="$cli --remote $(shimmer_quote "ssh-remote+$target")"
fi
shimmer_hyprland_launch "$EDITOR_PLACE" "$command" "the $REMOTE_EDITOR window connected to the VM" editor
