# Supervised: stop with one clear sentence before anything starts.
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"
. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-hyprland.sh"

problem=$(shimmer_display_problem)
[ -z "$problem" ] || shimmer_fail "$problem"

if [ "$VM_SOFTWARE" = "custom" ]; then
    [ -n "$START_COMMAND" ] || shimmer_fail "VM_SOFTWARE is custom, so it needs START_COMMAND: shimmer workspaces reconfigure $SHIMMER_WORKSPACE_ID --set START_COMMAND='…'"
else
    [ -n "$(shimmer_vm_cli "$VM_SOFTWARE")" ] || shimmer_fail "$VM_SOFTWARE isn't installed (or its command-line tool isn't on PATH)"
    shimmer_vm_exists || shimmer_fail "$VM_SOFTWARE has no VM called '$VM_NAME'"
fi

if [ "$REMOTE_EDITOR" != "none" ] || [ "${TERMINAL_APP:-none}" != "none" ]; then
    [ -n "$VM_HOST" ] || shimmer_fail "set VM_HOST (the VM's address) to open an editor or terminal inside it"
    shimmer_has ssh || shimmer_fail "ssh isn't installed"
fi
if [ "$VM_HOST" = "auto" ] && [ "$VM_SOFTWARE" != "utm" ] && [ "$VM_SOFTWARE" != "multipass" ]; then
    shimmer_fail "VM_HOST = auto only works with UTM and Multipass: give the VM's address"
fi
if [ "$REMOTE_EDITOR" != "none" ]; then
    [ -n "$(shimmer_remote_editor_cli "$REMOTE_EDITOR")" ] || shimmer_fail "$REMOTE_EDITOR isn't installed"
fi
shimmer_has_terminal "${TERMINAL_APP:-none}" || shimmer_fail "the terminal '$TERMINAL_APP' isn't installed here"

# Every *_PLACE answer: a valid "WORKSPACE:KEY" shape, and its monitor question filled in. A
# typo here should stop the launch, not silently open unplaced.
for pair in "EDITOR_PLACE:$EDITOR_PLACE" "TERMINAL_PLACE:$TERMINAL_PLACE"; do
    name=${pair%%:*}
    value=${pair#*:}
    [ -n "$value" ] || continue
    shimmer_hyprland_place_ok "$value" ||
        shimmer_fail "$name must look like 1:main or 4:second (a workspace number, a colon, then main or second), got '$value'"
    key=${value#*:}
    case "$key" in
        main) [ -n "$HYPRLAND_MAIN_MONITOR" ] || shimmer_fail "$name uses the main monitor, but HYPRLAND_MAIN_MONITOR is empty" ;;
        second) [ -n "$HYPRLAND_SECOND_MONITOR" ] || shimmer_fail "$name uses the second monitor, but HYPRLAND_SECOND_MONITOR is empty" ;;
    esac
done

if [ -n "$SSH_PASSWORD_ITEM" ]; then
    if shimmer_macos; then
        shimmer_has security || shimmer_fail "macOS's security tool is missing"
    else
        shimmer_has secret-tool || shimmer_fail "secret-tool isn't installed (it's in libsecret-tools)"
    fi
    shimmer_secret "$SSH_PASSWORD_ITEM" >/dev/null 2>&1 ||
        shimmer_fail "no keychain entry '$SSH_PASSWORD_ITEM'; save it once with: $(shimmer_secret_how_to_save "$SSH_PASSWORD_ITEM")"
fi
echo "ok: $VM_SOFTWARE VM '$VM_NAME'${VM_HOST:+, ssh to $VM_HOST}"
