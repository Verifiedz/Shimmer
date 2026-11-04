# Detached: Virtual Machine Manager (virt-manager), on VM_NAME's console. virt-manager starts
# the VM itself if it isn't running yet, so there's nothing else to start here.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

[ -n "$VM_NAME" ] || exit 0
shimmer_hyprland_launch "$VM_PLACE" "virt-manager --show-domain-console $(shimmer_quote "$VM_NAME")" "Virtual Machine Manager ($VM_NAME)" vm
