# Cleanup (supervised): suspend or shut down the VM, as ON_STOP says, but only a VM this
# workspace started (one you started yourself is left as it is). Run by `workspaces stop`, and by
# `workspaces cleanup` after a failed launch. Closes the SSH terminal (and virt-viewer) it opened
# when it can (CLOSE_ON_STOP), and the editor and terminal windows Hyprland placed (EDITOR_PLACE /
# TERMINAL_PLACE), by their exact remembered address; lists what it closed and what it left open.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

# Windows first: an SSH terminal into a VM that's about to be suspended is no use.
shimmer_close_windows
shimmer_hyprland_close_all
rm -f "$SHIMMER_STATE_DIR/askpass"
started="$SHIMMER_STATE_DIR/vm.started"
if [ "${ON_STOP:-leave-running}" = "leave-running" ]; then
    echo "leaving $VM_NAME running (ON_STOP = leave-running)"
    rm -f "$started"
    exit 0
fi
if [ ! -f "$started" ]; then
    echo "leaving $VM_NAME as it is: this workspace didn't start it"
    exit 0
fi
if [ "$VM_SOFTWARE" != "custom" ] && ! shimmer_vm_running; then
    echo "$VM_NAME isn't running"
    rm -f "$started"
    exit 0
fi
shimmer_vm_stop || shimmer_fail "$VM_SOFTWARE couldn't $ON_STOP '$VM_NAME' (its message is above)"
rm -f "$started"
echo "$ON_STOP: $VM_NAME"
