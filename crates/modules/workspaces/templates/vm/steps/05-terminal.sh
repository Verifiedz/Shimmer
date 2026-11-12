# Detached: a terminal on this computer, logged into the VM over SSH, landing in REMOTE_FOLDER
# when it's set (shimmer_ssh_command), the VM's home folder otherwise. With no key and no
# keychain entry, ssh asks for the password in that window: Shimmer never sees it.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"

[ "${TERMINAL_APP:-none}" = "none" ] && exit 0
host=$(shimmer_vm_host)
[ -n "$host" ] || exit 0
shimmer_open_terminal "$TERMINAL_APP" "$HOME" "$(shimmer_ssh_command "$(shimmer_ssh_target "$host")")"
