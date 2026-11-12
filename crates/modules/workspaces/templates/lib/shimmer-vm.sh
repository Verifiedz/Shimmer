# Helpers for the vm template (ADR 0025): one place that knows each VM program's commands.
# Steps load it after shimmer-open.sh:
#   . "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
#   . "$SHIMMER_WORKSPACE_DIR/lib/shimmer-vm.sh"
#
# POSIX sh only. This is your copy: Shimmer never changes it after the workspace is created.

# The command-line tool for VM_SOFTWARE: on PATH, or where the app installs it on macOS.
shimmer_vm_cli() {
    case "$1" in
        utm) set -- utmctl "/Applications/UTM.app/Contents/MacOS/utmctl" ;;
        parallels) set -- prlctl "/usr/local/bin/prlctl" ;;
        virtualbox) set -- VBoxManage "/Applications/VirtualBox.app/Contents/MacOS/VBoxManage" ;;
        vmware) set -- vmrun "/Applications/VMware Fusion.app/Contents/Library/vmrun" ;;
        libvirt) set -- virsh ;;
        multipass) set -- multipass ;;
        *) return 0 ;;
    esac
    shimmer_has "$1" && {
        echo "$1"
        return
    }
    [ -n "$2" ] && [ -x "$2" ] && echo "$2"
}

# Does the VM exist, as far as its software knows?
shimmer_vm_exists() {
    cli=$(shimmer_vm_cli "$VM_SOFTWARE")
    case "$VM_SOFTWARE" in
        utm) "$cli" list 2>/dev/null | grep -qF "$VM_NAME" ;;
        parallels) "$cli" list -a 2>/dev/null | grep -qF "$VM_NAME" ;;
        virtualbox) "$cli" showvminfo "$VM_NAME" >/dev/null 2>&1 ;;
        vmware) [ -f "$VM_NAME" ] ;;
        libvirt) "$cli" dominfo "$VM_NAME" >/dev/null 2>&1 ;;
        multipass) "$cli" info "$VM_NAME" >/dev/null 2>&1 ;;
        *) return 0 ;;
    esac
}

# Is it running already? Unknown (custom) counts as not running, so its start command runs.
shimmer_vm_running() {
    cli=$(shimmer_vm_cli "$VM_SOFTWARE")
    case "$VM_SOFTWARE" in
        utm) "$cli" status "$VM_NAME" 2>/dev/null | grep -qi "started" ;;
        parallels) "$cli" status "$VM_NAME" 2>/dev/null | grep -q "running" ;;
        virtualbox) "$cli" showvminfo "$VM_NAME" --machinereadable 2>/dev/null | grep -q '^VMState="running"' ;;
        vmware) "$cli" list 2>/dev/null | grep -qF "$VM_NAME" ;;
        libvirt) "$cli" domstate "$VM_NAME" 2>/dev/null | grep -q "running" ;;
        multipass) "$cli" info "$VM_NAME" 2>/dev/null | grep -q "State:.*Running" ;;
        *) return 1 ;;
    esac
}

# Start it, with its window or headless (START_MODE). UTM, Parallels and Multipass decide for
# themselves whether a window shows; the others are told.
shimmer_vm_start() {
    cli=$(shimmer_vm_cli "$VM_SOFTWARE")
    headless=false
    [ "$START_MODE" = "headless" ] && headless=true
    case "$VM_SOFTWARE" in
        utm) "$cli" start "$VM_NAME" ;;
        parallels) "$cli" start "$VM_NAME" ;;
        virtualbox)
            if $headless; then "$cli" startvm "$VM_NAME" --type headless; else "$cli" startvm "$VM_NAME" --type gui; fi
            ;;
        vmware)
            if $headless; then "$cli" start "$VM_NAME" nogui; else "$cli" start "$VM_NAME" gui; fi
            ;;
        libvirt)
            "$cli" start "$VM_NAME" || return 1
            if ! $headless && shimmer_has virt-viewer; then
                virt-viewer --attach "$VM_NAME" >/dev/null 2>&1 &
                shimmer_record_closable "the virt-viewer window"
            fi
            ;;
        multipass) "$cli" start "$VM_NAME" ;;
        custom) sh -c "$START_COMMAND" ;;
    esac
}

# Suspend, shut down or leave it running (ON_STOP), for `workspaces stop` and cleanup.
shimmer_vm_stop() {
    cli=$(shimmer_vm_cli "$VM_SOFTWARE")
    case "$ON_STOP:$VM_SOFTWARE" in
        leave-running:*) return 0 ;;
        *:custom) [ -n "$STOP_COMMAND" ] && sh -c "$STOP_COMMAND" ;;
        suspend:utm) "$cli" suspend "$VM_NAME" ;;
        shutdown:utm) "$cli" stop "$VM_NAME" ;;
        suspend:parallels) "$cli" suspend "$VM_NAME" ;;
        shutdown:parallels) "$cli" stop "$VM_NAME" ;;
        suspend:virtualbox) "$cli" controlvm "$VM_NAME" savestate ;;
        shutdown:virtualbox) "$cli" controlvm "$VM_NAME" acpipowerbutton ;;
        suspend:vmware) "$cli" suspend "$VM_NAME" ;;
        shutdown:vmware) "$cli" stop "$VM_NAME" soft ;;
        suspend:libvirt) "$cli" managedsave "$VM_NAME" ;;
        shutdown:libvirt) "$cli" shutdown "$VM_NAME" ;;
        suspend:multipass) "$cli" suspend "$VM_NAME" ;;
        shutdown:multipass) "$cli" stop "$VM_NAME" ;;
    esac
}

# The address to SSH to: VM_HOST, or with VM_HOST = auto, asked of UTM or Multipass.
shimmer_vm_host() {
    [ "$VM_HOST" = "auto" ] || {
        echo "$VM_HOST"
        return
    }
    cli=$(shimmer_vm_cli "$VM_SOFTWARE")
    case "$VM_SOFTWARE" in
        utm) "$cli" ip-address "$VM_NAME" 2>/dev/null | head -n 1 ;;
        multipass) "$cli" info "$VM_NAME" 2>/dev/null | awk '/IPv4/ { print $2; exit }' ;;
    esac
}

# user@host, or just host.
shimmer_ssh_target() {
    host=$1
    if [ -n "$SSH_USER" ]; then echo "$SSH_USER@$host"; else echo "$host"; fi
}

# Does something answer on the VM's SSH port? `nc` when there is one, else ssh itself: a
# refused or timed-out connection is "not yet", anything else (even "Permission denied") is up.
shimmer_port_open() {
    if shimmer_has nc; then
        nc -z -w 2 "$1" "$2" >/dev/null 2>&1
        return
    fi
    out=$(ssh -o BatchMode=yes -o ConnectTimeout=3 -o StrictHostKeyChecking=accept-new -p "$2" "$1" true 2>&1) && return 0
    case "$out" in
        *"Connection refused"* | *"timed out"* | *"No route to host"* | *"Could not resolve"*) return 1 ;;
        *) return 0 ;;
    esac
}

# ---------------------------------------------------------------- the password, never stored

# Read the secret saved under SSH_PASSWORD_ITEM from the system keychain. The workspace only ever
# knows the entry's name; the password stays in macOS Keychain or the Linux keyring.
shimmer_secret() {
    if shimmer_macos; then
        security find-generic-password -s "$1" -w
    else
        secret-tool lookup shimmer "$1"
    fi
}

# How to save it, for error messages.
shimmer_secret_how_to_save() {
    if shimmer_macos; then
        echo "security add-generic-password -s '$1' -a \"\$USER\" -w"
    else
        echo "secret-tool store --label='$1' shimmer '$1'"
    fi
}

# Write a tiny SSH_ASKPASS program that reads the keychain entry, into this workspace's temp
# folder (never the Shimmer folder), and print its path. It holds the entry's name, not the
# password, and is readable by you alone.
shimmer_askpass() {
    item=$1
    mkdir -p "$SHIMMER_STATE_DIR" && chmod 700 "$SHIMMER_STATE_DIR"
    file="$SHIMMER_STATE_DIR/askpass"
    if shimmer_macos; then
        printf '#!/bin/sh\nexec security find-generic-password -s %s -w\n' "$(shimmer_quote "$item")" >"$file"
    else
        printf '#!/bin/sh\nexec secret-tool lookup shimmer %s\n' "$(shimmer_quote "$item")" >"$file"
    fi
    chmod 700 "$file"
    echo "$file"
}

# The ssh command for a terminal: plain `ssh` (a key, or ssh asks you itself), or with the
# keychain entry handed to ssh through SSH_ASKPASS. Lands in REMOTE_FOLDER when it's set, the
# same question the editor step already uses, instead of always the VM's home folder, then runs
# REMOTE_COMMAND (e.g. `claude --resume`) before dropping to a login shell.
shimmer_ssh_command() {
    target=$1
    cmd="ssh -p $(shimmer_quote "${SSH_PORT:-22}") $(shimmer_quote "$target")"
    if [ -n "$REMOTE_FOLDER" ] || [ -n "$REMOTE_COMMAND" ]; then
        remote=""
        [ -n "$REMOTE_FOLDER" ] && remote="cd $(shimmer_quote "$REMOTE_FOLDER") && "
        [ -n "$REMOTE_COMMAND" ] && remote="$remote$REMOTE_COMMAND; "
        remote="${remote}exec \$SHELL -l"
        cmd="$cmd -t $(shimmer_quote "$remote")"
    fi
    if [ -n "$SSH_PASSWORD_ITEM" ]; then
        askpass=$(shimmer_askpass "$SSH_PASSWORD_ITEM")
        cmd="SSH_ASKPASS=$(shimmer_quote "$askpass") SSH_ASKPASS_REQUIRE=force $cmd"
    fi
    echo "$cmd"
}

# The CLI for an editor that can open a folder on another machine over SSH (Remote-SSH): on
# PATH, or inside the macOS app bundle when its shell command isn't installed.
shimmer_remote_editor_cli() {
    case "$1" in
        cursor) set -- cursor "/Applications/Cursor.app/Contents/Resources/app/bin/cursor" ;;
        vscode) set -- code "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code" ;;
        *) return 0 ;;
    esac
    shimmer_has "$1" && {
        echo "$1"
        return
    }
    [ -x "$2" ] && echo "$2"
}
