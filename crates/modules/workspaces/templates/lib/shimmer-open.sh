# Shared helpers for workspace templates (ADR 0025 §5). Steps load it with
#   . "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"
#
# Everything that differs between Linux and macOS lives here, so step scripts stay short.
# POSIX sh only: on Debian and Ubuntu `sh` is dash (ADR 0012 §5).
#
# This is your copy: Shimmer never changes it after the workspace is created.

shimmer_has() {
    command -v "$1" >/dev/null 2>&1
}

shimmer_macos() {
    [ "$SHIMMER_PLATFORM" = "macos" ]
}

# Print an error for the step's log and stop the step. In a supervised step this marks the
# workspace dirty with this message.
shimmer_fail() {
    echo "$*" >&2
    exit 1
}

# Where this workspace keeps its dev server's log and process ids: the system temp folder, never
# inside the Shimmer folder (only the daemon writes there). A reboot clears it, and a reboot
# also ends the server. Named after this user and this Shimmer folder as well as the workspace,
# so two Shimmer folders or two users with a same-named workspace never share it (#165).
shimmer_home_tag() {
    LC_ALL=C awk 'BEGIN {
        for (i = 1; i < 256; i++) code[sprintf("%c", i)] = i
        s = ARGV[1]; h = 0
        for (i = 1; i <= length(s); i++) h = (h * 31 + code[substr(s, i, 1)]) % 2147483647
        printf "%08x", h
    }' "$SHIMMER_HOME"
}
SHIMMER_STATE_DIR="${TMPDIR:-/tmp}/shimmer-$(id -u)-$(shimmer_home_tag)-$SHIMMER_WORKSPACE_ID"
# Private, and ours: one that already exists as anything else (another user's folder, a link)
# could make stop signal a process this workspace never started, so it's refused. Written so a
# shell that can't run a test refuses rather than accepts.
mkdir -m 700 "$SHIMMER_STATE_DIR" 2>/dev/null
if ! { [ ! -L "$SHIMMER_STATE_DIR" ] && [ -d "$SHIMMER_STATE_DIR" ] && [ -O "$SHIMMER_STATE_DIR" ]; }; then
    if [ ! -d "${TMPDIR:-/tmp}" ]; then
        shimmer_fail "can't keep this workspace's run-time files: ${TMPDIR:-/tmp} doesn't exist (check TMPDIR)"
    fi
    owner=$(ls -ld "$SHIMMER_STATE_DIR" 2>/dev/null | awk '{print $3}')
    shimmer_fail "can't use $SHIMMER_STATE_DIR for this workspace's run-time files: something else is there (owned by ${owner:-someone else}), not a folder of your own. Remove it if you can; a reboot clears it."
fi
chmod 700 "$SHIMMER_STATE_DIR"

# ---------------------------------------------------------------- where windows open

# Where editors, browsers and terminals appear (OPEN_ON). Steps inherit the daemon's settings,
# which come from whatever terminal last started it: a Cursor or VS Code terminal connected from
# another computer sends windows to that computer, the machine's own desktop keeps them here. So
# a workspace can say where it wants them instead:
#   auto                as the daemon was started (unchanged)
#   this-machine        this computer's own screen, ignoring any editor's remote-connection helpers
#   connected-computer  the computer you're connected from, through Cursor's or VS Code's helpers
shimmer_use_display() {
    case "${OPEN_ON:-auto}" in
        this-machine)
            unset BROWSER VSCODE_IPC_HOOK_CLI
            PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -v -e '/\.cursor-server/' -e '/\.vscode-server/' | paste -s -d: -)
            export PATH
            shimmer_macos && return 0
            XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
            export XDG_RUNTIME_DIR
            if [ -z "$WAYLAND_DISPLAY" ]; then
                for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
                    case "$sock" in *.lock) continue ;; esac
                    if [ -S "$sock" ]; then
                        WAYLAND_DISPLAY=$(basename "$sock")
                        export WAYLAND_DISPLAY
                        break
                    fi
                done
            fi
            if [ -z "$DISPLAY" ] && [ -S /tmp/.X11-unix/X0 ]; then
                DISPLAY=:0
                export DISPLAY
            fi
            if [ -z "$DBUS_SESSION_BUS_ADDRESS" ] && [ -S "$XDG_RUNTIME_DIR/bus" ]; then
                DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
                export DBUS_SESSION_BUS_ADDRESS
            fi
            ;;
        connected-computer)
            bin=$(ls -td "$HOME"/.cursor-server/bin/*/*/bin "$HOME"/.cursor-server/bin/*/bin \
                "$HOME"/.vscode-server/bin/*/bin "$HOME"/.vscode-server/cli/servers/*/server/bin 2>/dev/null | head -n 1)
            [ -n "$bin" ] || return 0
            [ -d "$bin/remote-cli" ] && PATH="$bin/remote-cli:$PATH" && export PATH
            [ -f "$bin/helpers/browser.sh" ] && BROWSER="$bin/helpers/browser.sh" && export BROWSER
            if [ -z "$VSCODE_IPC_HOOK_CLI" ]; then
                rt=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
                VSCODE_IPC_HOOK_CLI=$(ls -t "$rt"/vscode-ipc-*.sock 2>/dev/null | head -n 1)
                [ -n "$VSCODE_IPC_HOOK_CLI" ] && export VSCODE_IPC_HOOK_CLI
            fi
            ;;
    esac
}

# Why windows can't open where OPEN_ON asks, or nothing. For the check step.
shimmer_display_problem() {
    case "${OPEN_ON:-auto}" in
        this-machine)
            shimmer_macos && return 0
            [ -n "$WAYLAND_DISPLAY" ] || [ -n "$DISPLAY" ] ||
                echo "OPEN_ON is this-machine, but this computer has no desktop session open: log in on its screen first"
            ;;
        connected-computer)
            [ -n "$VSCODE_IPC_HOOK_CLI" ] && [ -S "$VSCODE_IPC_HOOK_CLI" ] ||
                echo "OPEN_ON is connected-computer, but no Cursor or VS Code window is connected to this computer right now"
            ;;
    esac
}

# ---------------------------------------------------------------- urls

# The default browser. When BROWSER is set it wins: over a remote session (Cursor or VS Code
# connected to this machine) it's the helper that opens the link on the computer you're sitting at.
shimmer_open_url() {
    for url in "$@"; do
        [ -n "$url" ] || continue
        if [ -n "$BROWSER" ]; then
            "$BROWSER" "$url" >/dev/null 2>&1 &
        elif shimmer_macos; then
            open "$url"
        elif shimmer_has xdg-open; then
            xdg-open "$url" >/dev/null 2>&1 &
        else
            echo "no xdg-open: can't open $url" >&2
        fi
    done
}

# ---------------------------------------------------------------- editors

# The shell commands an editor may be installed as on Linux, best first.
shimmer_editor_commands() {
    case "$1" in
        vscode) echo "code codium" ;;
        cursor) echo "cursor" ;;
        zed) echo "zed zeditor zedit" ;;
        webstorm) echo "webstorm" ;;
        intellij) echo "idea idea-ultimate idea-community intellij-idea-ultimate intellij-idea-community" ;;
        sublime) echo "subl" ;;
        neovim) echo "nvim" ;;
        *) echo "" ;;
    esac
}

# The app name on macOS.
shimmer_editor_app() {
    case "$1" in
        vscode) echo "Visual Studio Code" ;;
        cursor) echo "Cursor" ;;
        zed) echo "Zed" ;;
        webstorm) echo "WebStorm" ;;
        intellij) echo "IntelliJ IDEA" ;;
        sublime) echo "Sublime Text" ;;
        *) echo "" ;;
    esac
}

# The first installed command for EDITOR, or nothing.
shimmer_editor_command() {
    for c in $(shimmer_editor_commands "$1"); do
        if shimmer_has "$c"; then
            echo "$c"
            return
        fi
    done
}

# Succeeds when EDITOR can be opened here.
shimmer_has_editor() {
    [ "$1" = "none" ] && return 0
    [ -n "$(shimmer_editor_command "$1")" ] && return 0
    if shimmer_macos; then
        app=$(shimmer_editor_app "$1")
        [ -n "$app" ] && open -Ra "$app" >/dev/null 2>&1 && return 0
        # IntelliJ's free edition has its own app name.
        [ "$1" = "intellij" ] && open -Ra "IntelliJ IDEA CE" >/dev/null 2>&1 && return 0
    fi
    return 1
}

# Open EDITOR on PATH (a folder or a file). Neovim runs in a terminal (TERMINAL_APP).
shimmer_open_editor() {
    editor=$1
    target=$2
    case "$editor" in
        none) return 0 ;;
        neovim)
            dir=$target
            [ -d "$dir" ] || dir=$(dirname "$target")
            shimmer_open_terminal "${TERMINAL_APP:-auto}" "$dir" "nvim $(shimmer_quote "$target")"
            return
            ;;
    esac
    cmd=$(shimmer_editor_command "$editor")
    if [ -n "$cmd" ]; then
        "$cmd" "$target" >/dev/null 2>&1 &
        return 0
    fi
    if shimmer_macos; then
        app=$(shimmer_editor_app "$editor")
        case "$editor" in
            # JetBrains apps take the project as an argument to a new instance.
            webstorm | intellij) open -na "$app" --args "$target" ;;
            *) open -a "$app" "$target" ;;
        esac
        return
    fi
    shimmer_fail "$editor isn't installed"
}

# ---------------------------------------------------------------- a terminal inside the editor

# A string as a JSON string literal.
shimmer_json_string() {
    printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"
}

# Make VS Code / Cursor open a terminal running COMMAND inside the editor whenever FOLDER opens:
# a task in FOLDER/.vscode/tasks.json with `"runOn": "folderOpen"`, the editors' own feature
# for this. Only ever writes or removes a tasks.json that Shimmer created (it says so on its
# first line); a tasks.json of yours is left alone and the task to add is printed instead.
# Kept out of git through .git/info/exclude, which is local and never committed. An empty
# COMMAND removes Shimmer's file.
shimmer_ide_terminal() {
    folder=$1
    command=$2
    file="$folder/.vscode/tasks.json"
    marker="// Created by Shimmer workspace '$SHIMMER_WORKSPACE_ID'"
    ours=false
    [ -f "$file" ] && head -n 1 "$file" | grep -qF "// Created by Shimmer workspace" && ours=true
    if [ -z "$command" ]; then
        if $ours; then
            rm -f "$file"
            echo "removed Shimmer's $file"
        fi
        return 0
    fi
    task=$(cat <<TASK
    {
      "label": $(shimmer_json_string "Shimmer: $command"),
      "type": "shell",
      "command": $(shimmer_json_string "$command"),
      "isBackground": true,
      "problemMatcher": [],
      "presentation": { "reveal": "always", "panel": "dedicated", "focus": false },
      "runOptions": { "runOn": "folderOpen" }
    }
TASK
)
    if [ -f "$file" ] && ! $ours; then
        echo "$file is yours, so Shimmer won't change it. To get the terminal, add this to its \"tasks\":"
        echo "$task"
        return 0
    fi
    mkdir -p "$folder/.vscode" || return 0
    {
        echo "$marker: opens a terminal in the editor"
        echo "// running your command when this folder opens. Change it with: shimmer workspaces reconfigure $SHIMMER_WORKSPACE_ID --set IDE_TERMINAL_COMMAND='…'"
        echo "// (IDE_TERMINAL_COMMAND). Shimmer rewrites this file on every launch; it's kept out of git."
        echo "{"
        echo '  "version": "2.0.0",'
        echo '  "tasks": ['
        echo "$task"
        echo "  ]"
        echo "}"
    } >"$file"
    # Hidden from `git status` on this machine only, unless the file is already tracked.
    if git -C "$folder" rev-parse --git-dir >/dev/null 2>&1 && ! git -C "$folder" ls-files --error-unmatch .vscode/tasks.json >/dev/null 2>&1; then
        exclude="$(git -C "$folder" rev-parse --git-path info/exclude)"
        case "$exclude" in /*) ;; *) exclude="$folder/$exclude" ;; esac
        mkdir -p "$(dirname "$exclude")"
        grep -qxF "/.vscode/tasks.json" "$exclude" 2>/dev/null || echo "/.vscode/tasks.json" >>"$exclude"
    fi
    echo "the editor will open a terminal running: $command"
}

# ---------------------------------------------------------------- terminals

# 'it'\''s' style quoting, so a path or command survives being passed through another shell.
shimmer_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# The terminal TERMINAL_APP=auto means: Terminal on macOS, the first one found on Linux.
shimmer_terminal_auto() {
    if shimmer_macos; then
        echo "terminal"
        return
    fi
    for t in kitty foot alacritty wezterm ghostty gnome-terminal konsole xterm; do
        if shimmer_has "$t"; then
            echo "$t"
            return
        fi
    done
}

# Succeeds when TERMINAL can be opened here.
shimmer_has_terminal() {
    case "$1" in
        none) return 0 ;;
        auto) [ -n "$(shimmer_terminal_auto)" ] ;;
        terminal) shimmer_macos ;;
        iterm) shimmer_macos && open -Ra "iTerm" >/dev/null 2>&1 ;;
        *) shimmer_has "$1" ;;
    esac
}

# Open TERMINAL in DIR, running COMMAND (a shell command line) if given. The terminal stays open
# afterwards, at a shell in DIR.
#
# Terminals that open each window as their own program (kitty, foot, alacritty, wezterm, ghostty,
# konsole, xterm) are started as a new program in this step's process group and recorded, so
# `workspaces stop` can close exactly that window. GNOME Terminal and macOS Terminal / iTerm hand
# every window to one shared program, so their windows can't be closed without closing your other
# ones: they're recorded as left open, and stop says so.
shimmer_open_terminal() {
    term=$1
    dir=$2
    command=$3
    [ "$term" = "auto" ] && term=$(shimmer_terminal_auto)
    [ -n "$term" ] || shimmer_fail "no terminal found: install one, or choose one with shimmer workspaces reconfigure $SHIMMER_WORKSPACE_ID --set TERMINAL_APP=…"
    [ "$term" = "none" ] && return 0
    if [ "${SHIMMER_REUSE_WINDOW:-yes}" = "yes" ] && shimmer_window_open; then
        echo "already open from the last activate: the $term terminal"
        return 0
    fi
    shell=${SHELL:-sh}
    # What runs inside the terminal: the command, then an interactive shell.
    if [ -n "$command" ]; then
        inner="cd $(shimmer_quote "$dir") && $command; exec $(shimmer_quote "$shell")"
    else
        inner="cd $(shimmer_quote "$dir") && exec $(shimmer_quote "$shell")"
    fi
    case "$term" in
        terminal | iterm)
            line=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
            if [ "$term" = "terminal" ]; then
                osascript -e "tell application \"Terminal\" to do script \"$line\"" -e 'tell application "Terminal" to activate' >/dev/null
                shimmer_record_left_open "the Terminal window (macOS Terminal keeps all its windows in one program)"
            else
                osascript -e 'tell application "iTerm"' -e 'set w to (create window with default profile)' \
                    -e "tell current session of w to write text \"$line\"" -e 'end tell' >/dev/null
                shimmer_record_left_open "the iTerm window (iTerm keeps all its windows in one program)"
            fi
            return 0
            ;;
        gnome-terminal)
            gnome-terminal --working-directory="$dir" -- "$shell" -c "$inner" >/dev/null 2>&1 &
            shimmer_record_left_open "the GNOME Terminal window (GNOME Terminal keeps all its windows in one program)"
            return 0
            ;;
        kitty) kitty --directory "$dir" "$shell" -c "$inner" >/dev/null 2>&1 & ;;
        foot) foot --working-directory="$dir" "$shell" -c "$inner" >/dev/null 2>&1 & ;;
        alacritty) alacritty --working-directory "$dir" -e "$shell" -c "$inner" >/dev/null 2>&1 & ;;
        wezterm) wezterm start --always-new-process --cwd "$dir" -- "$shell" -c "$inner" >/dev/null 2>&1 & ;;
        ghostty)
            if shimmer_macos; then
                ghostty --working-directory="$dir" -e "$shell" -c "$inner" >/dev/null 2>&1 &
            else
                ghostty --gtk-single-instance=false --working-directory="$dir" -e "$shell" -c "$inner" >/dev/null 2>&1 &
            fi
            ;;
        konsole) konsole --nofork --workdir "$dir" -e "$shell" -c "$inner" >/dev/null 2>&1 & ;;
        *) (cd "$dir" && exec "$term" -e "$shell" -c "$inner") >/dev/null 2>&1 & ;;
    esac
    shimmer_record_closable "the $term terminal"
    return 0
}

# ---------------------------------------------------------------- what stop can close

# A window this step started as its own program: its process group (this step's own, since every
# detached step is a group of its own, ADR 0010 §3) and what to call it.
#   Each line: the process group, the step that opened it, what to call it.
shimmer_record_closable() {
    mkdir -p "$SHIMMER_STATE_DIR"
    echo "$$ $(basename "$0") $1" >>"$SHIMMER_STATE_DIR/closable"
}

# Is the window this step opened on an earlier activate still open? Activating an active
# workspace again then opens nothing new, as with its dev server, instead of piling up windows.
shimmer_window_open() {
    file="$SHIMMER_STATE_DIR/closable"
    [ -f "$file" ] || return 1
    for pgid in $(awk -v step="$(basename "$0")" '$2 == step { print $1 }' "$file"); do
        kill -s 0 -- "-$pgid" 2>/dev/null && return 0
    done
    return 1
}

# Something this workspace opened that stop can't close, and why.
shimmer_record_left_open() {
    mkdir -p "$SHIMMER_STATE_DIR"
    echo "$1" >>"$SHIMMER_STATE_DIR/left-open"
}

# For cleanup: close what can be closed (CLOSE_ON_STOP = yes), and say exactly what was closed and
# what was left open.
shimmer_close_windows() {
    closable="$SHIMMER_STATE_DIR/closable"
    left="$SHIMMER_STATE_DIR/left-open"
    if [ -f "$closable" ]; then
        while read -r pgid _ label; do
            [ -n "$pgid" ] || continue
            if ! kill -s 0 -- "-$pgid" 2>/dev/null; then
                echo "already closed: $label"
            elif [ "${CLOSE_ON_STOP:-yes}" = "yes" ]; then
                shimmer_stop_group "$pgid"
                echo "closed: $label"
            else
                echo "left open (CLOSE_ON_STOP = no): $label"
            fi
        done <"$closable"
        rm -f "$closable" "$SHIMMER_STATE_DIR/browser.pgid"
    fi
    if [ -f "$left" ]; then
        sort -u "$left" | while read -r what; do
            echo "left open, close it yourself: $what"
        done
        rm -f "$left"
    fi
}

# ---------------------------------------------------------------- a browser window of its own

# The browser program for BROWSER_APP (auto: the first one found), for a window of its own.
shimmer_browser_bin() {
    case "$1" in
        firefox) set -- firefox firefox-esr "/Applications/Firefox.app/Contents/MacOS/firefox" ;;
        chrome) set -- google-chrome google-chrome-stable "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ;;
        chromium) set -- chromium chromium-browser "/Applications/Chromium.app/Contents/MacOS/Chromium" ;;
        brave) set -- brave-browser brave "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser" ;;
        edge) set -- microsoft-edge microsoft-edge-stable "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" ;;
        *)
            for app in firefox chrome chromium brave edge; do
                found=$(shimmer_browser_bin "$app")
                [ -n "$found" ] && echo "$found" && return
            done
            return 0
            ;;
    esac
    for candidate in "$@"; do
        case "$candidate" in
            /*) [ -x "$candidate" ] && echo "$candidate" && return ;;
            *) shimmer_has "$candidate" && echo "$candidate" && return ;;
        esac
    done
}

# Open URLs in a browser window of the workspace's own: a separate copy of the browser with its
# own profile, kept between launches in ~/.local/state/shimmer/browser-profiles/<id> so logins
# stick, and closed by stop without touching your normal browser. Opening again while it's still
# up adds tabs to that same window.
shimmer_open_urls_window() {
    app=$1
    shift
    bin=$(shimmer_browser_bin "$app")
    if [ -z "$bin" ]; then
        echo "no browser found for BROWSER_APP=$app: opening in your normal browser instead"
        shimmer_open_url "$@"
        return
    fi
    profile="${XDG_STATE_HOME:-$HOME/.local/state}/shimmer/browser-profiles/$SHIMMER_WORKSPACE_ID"
    mkdir -p "$profile" && chmod 700 "$profile"
    running=""
    pgid_file="$SHIMMER_STATE_DIR/browser.pgid"
    [ -f "$pgid_file" ] && kill -s 0 -- "-$(cat "$pgid_file")" 2>/dev/null && running=yes
    if [ -n "$running" ]; then
        # Still open from the last activate, with its tabs: opening them again would only
        # duplicate them.
        echo "already open from the last activate: the $(basename "$bin") window with this workspace's tabs"
        return 0
    fi
    case "$(basename "$bin")" in
        firefox*)
            # No "restore session?" after stop closes it, and no default-browser nag.
            [ -f "$profile/user.js" ] || printf '%s\n' \
                'user_pref("browser.sessionstore.resume_from_crash", false);' \
                'user_pref("browser.shell.checkDefaultBrowser", false);' >"$profile/user.js"
            "$bin" --new-instance --profile "$profile" "$@" >/dev/null 2>&1 &
            ;;
        *) "$bin" --user-data-dir="$profile" --no-first-run --hide-crash-restore-bubble --new-window "$@" >/dev/null 2>&1 & ;;
    esac
    mkdir -p "$SHIMMER_STATE_DIR" && echo "$$" >"$pgid_file"
    shimmer_record_closable "the $(basename "$bin") window with this workspace's tabs"
}

# ---------------------------------------------------------------- web projects

# The package manager a project uses, from its lockfile; nothing without a package.json.
shimmer_package_manager() {
    dir=$1
    [ -f "$dir/package.json" ] || return 0
    if [ -f "$dir/pnpm-lock.yaml" ]; then
        echo pnpm
    elif [ -f "$dir/yarn.lock" ]; then
        echo yarn
    elif [ -f "$dir/bun.lockb" ] || [ -f "$dir/bun.lock" ]; then
        echo bun
    else
        echo npm
    fi
}

shimmer_lockfile() {
    case "$1" in
        pnpm) echo pnpm-lock.yaml ;;
        yarn) echo yarn.lock ;;
        bun) if [ -f "$2/bun.lock" ]; then echo bun.lock; else echo bun.lockb; fi ;;
        npm) echo package-lock.json ;;
    esac
}

# Does package.json have this script?
shimmer_has_script() {
    grep -q "\"$2\"[[:space:]]*:" "$1/package.json" 2>/dev/null
}

# Does package.json name this package (as a dependency)?
shimmer_has_package() {
    grep -q "\"$2\"[[:space:]]*:" "$1/package.json" 2>/dev/null
}

# DEV_COMMAND, with `auto` worked out: `<pm> run dev`, else `<pm> start`, else nothing.
shimmer_dev_command() {
    dir=$1
    answer=$2
    [ "$answer" = "auto" ] || {
        echo "$answer"
        return
    }
    pm=$(shimmer_package_manager "$dir")
    [ -n "$pm" ] || return 0
    if shimmer_has_script "$dir" dev; then
        echo "$pm run dev"
    elif shimmer_has_script "$dir" start; then
        echo "$pm start"
    fi
}

# LOCAL_URL, with `auto` worked out from the framework the project uses.
shimmer_local_url() {
    dir=$1
    answer=$2
    [ "$answer" = "auto" ] || {
        echo "$answer"
        return
    }
    for pair in next:3000 nuxt:3000 react-scripts:3000 astro:4321 gatsby:8000 @sveltejs/kit:5173 vite:5173; do
        if shimmer_has_package "$dir" "${pair%%:*}"; then
            echo "http://localhost:${pair##*:}"
            return
        fi
    done
}

# LIVE_URL, with `auto` read from the `homepage` field of the project's package.json, the
# standard place a project says where it's deployed. Nothing when there's no such field.
shimmer_live_url() {
    dir=$1
    answer=$2
    [ "$answer" = "auto" ] || {
        echo "$answer"
        return
    }
    [ -f "$dir/package.json" ] || return 0
    sed -n 's/.*"homepage"[[:space:]]*:[[:space:]]*"\(https\{0,1\}:\/\/[^"]*\)".*/\1/p' "$dir/package.json" | head -n 1
}

# Run a command line in DIR with the Node version the project asks for (.nvmrc or
# .node-version), when fnm or nvm is installed. Otherwise as it is.
shimmer_run_in_project() {
    dir=$1
    command=$2
    cd "$dir" || shimmer_fail "can't enter $dir"
    if [ -f .nvmrc ] || [ -f .node-version ]; then
        if shimmer_has fnm; then
            eval "$(fnm env)" && fnm use --install-if-missing >/dev/null 2>&1
        elif [ -s "${NVM_DIR:-$HOME/.nvm}/nvm.sh" ]; then
            . "${NVM_DIR:-$HOME/.nvm}/nvm.sh" && nvm use >/dev/null 2>&1
        fi
    fi
    sh -c "$command"
}

# The project's repository page, from `git remote get-url origin`, as https. PAGE is home,
# pulls, issues or actions; the last three only on GitHub.
shimmer_repo_url() {
    dir=$1
    page=$2
    [ "$page" = "none" ] && return 0
    remote=$(git -C "$dir" remote get-url origin 2>/dev/null) || return 0
    case "$remote" in
        git@*:*) url="https://$(printf '%s' "${remote#git@}" | sed 's/:/\//')" ;;
        ssh://*) url="https://$(printf '%s' "${remote#ssh://}" | sed 's/^[^@]*@//; s/:[0-9]*\//\//')" ;;
        https://* | http://*) url=$(printf '%s' "$remote" | sed 's#//[^@/]*@#//#') ;;
        *) return 0 ;;
    esac
    url=${url%.git}
    case "$page:$url" in
        home:*) echo "$url" ;;
        pulls:https://github.com/* | issues:https://github.com/* | actions:https://github.com/*) echo "$url/$page" ;;
        *) echo "$url" ;;
    esac
}

# Does something answer at URL?
shimmer_answers() {
    curl -s -o /dev/null --max-time 2 "$1"
}

# The process group of this workspace's dev server(s), if anything in it is still running. The
# whole group, not just the step that started it: the step may have exited while its servers run.
shimmer_dev_server_pid() {
    file="$SHIMMER_STATE_DIR/dev-server.pid"
    [ -f "$file" ] || return 0
    pid=$(cat "$file")
    [ -n "$pid" ] && kill -s 0 -- "-$pid" 2>/dev/null && echo "$pid"
}

# Stop a process and everything it started: a detached step is its own process group.
shimmer_stop_group() {
    pid=$1
    kill -s TERM -- "-$pid" 2>/dev/null || kill -s TERM "$pid" 2>/dev/null
    i=0
    while kill -0 "$pid" 2>/dev/null && [ $i -lt 10 ]; do
        sleep 0.5
        i=$((i + 1))
    done
    kill -s KILL -- "-$pid" 2>/dev/null
    return 0
}


# ---------------------------------------------------------------- a command that needs you

# Run COMMAND in a new terminal window (TERMINAL_APP) and wait until it ends, for something you
# must type into yourself, like sudo's password: Shimmer never sees it. NAME names its files in
# the temp folder. Returns the command's exit code, or 254 when the window was closed before it
# finished; fails the step when no window opens within 30 seconds.
shimmer_terminal_and_wait() {
    name=$1
    cmd=$2
    mkdir -p "$SHIMMER_STATE_DIR"
    pid_file="$SHIMMER_STATE_DIR/$name.pid"
    exit_file="$SHIMMER_STATE_DIR/$name.exit"
    rm -f "$pid_file" "$exit_file"
    # The window records its process id when it starts and the exit code when it ends.
    inner="echo \$\$ > $(shimmer_quote "$pid_file"); echo $(shimmer_quote "\$ $cmd"); $cmd; code=\$?; echo \$code > $(shimmer_quote "$exit_file")"
    inner="$inner; echo; echo 'Done: this window closes in 10 seconds.'; sleep 10; exit"
    # Always a new window: this one exists to run the command now.
    SHIMMER_REUSE_WINDOW=no shimmer_open_terminal "${TERMINAL_APP:-auto}" "$HOME" "$inner"
    echo "opened a terminal for: $cmd"
    waited=0
    while [ ! -f "$exit_file" ]; do
        sleep 1
        waited=$((waited + 1))
        if [ -f "$pid_file" ]; then
            kill -s 0 "$(cat "$pid_file")" 2>/dev/null || [ -f "$exit_file" ] || return 254
        elif [ "$waited" -ge 30 ]; then
            shimmer_fail "the terminal for '$cmd' didn't open within 30 seconds"
        fi
    done
    return "$(cat "$exit_file")"
}

# Every step that loads this file opens things where OPEN_ON says.
shimmer_use_display
