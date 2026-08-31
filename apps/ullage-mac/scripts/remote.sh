#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "__signed-launcher" ]]; then
    if [[ $# -ne 7 ]]; then
        echo "invalid signed launcher invocation" >&2
        exit 2
    fi
    package_dir="$2"
    action="$3"
    identity="$4"
    profile="$5"
    log_file="$6"
    status_file="$7"
    if [[ "$action" != "sign" && "$action" != "notarize" ]]; then
        echo "invalid signed launcher action" >&2
        exit 2
    fi
    export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
    cd "$package_dir"
    set +e
    if [[ "$action" == "sign" ]]; then
        make bundle SIGN_IDENTITY="$identity" >"$log_file" 2>&1
    else
        make notarize SIGN_IDENTITY="$identity" NOTARY_PROFILE="$profile" >"$log_file" 2>&1
    fi
    status=$?
    printf '%s\n' "$status" >"$status_file.tmp"
    mv -f "$status_file.tmp" "$status_file"
    exit "$status"
fi

if [[ -z "${ULLAGE_MAC_SSH:-}" ]]; then
    echo "ULLAGE_MAC_SSH is required" >&2
    exit 2
fi

if [[ $# -ne 1 || ! "$1" =~ ^(test|build|bundle|run|sign|notarize)$ ]]; then
    echo "usage: $0 <test|build|bundle|run|sign|notarize>" >&2
    exit 2
fi
action="$1"

shell_join() {
    local result="" argument
    for argument in "$@"; do
        printf -v result '%s%q ' "$result" "$argument"
    done
    printf '%s' "${result% }"
}

remote_exec() {
    local command wrapped
    command="$(shell_join "$@")"
    wrapped="$(shell_join /bin/bash --norc -c "$command")"
    ssh "$ULLAGE_MAC_SSH" "$wrapped"
}

if [[ "$action" == "sign" || "$action" == "notarize" ]]; then
    if [[ -z "${ULLAGE_MAC_GUI_TMUX_SESSION:-}" ]]; then
        echo "ULLAGE_MAC_GUI_TMUX_SESSION is required for $action" >&2
        exit 2
    fi
    if [[ ! "$ULLAGE_MAC_GUI_TMUX_SESSION" =~ ^[A-Za-z0-9_-]+$ ]]; then
        echo "ULLAGE_MAC_GUI_TMUX_SESSION must contain only letters, digits, underscores, and hyphens" >&2
        exit 2
    fi
    if [[ -z "${ULLAGE_MAC_SIGN_IDENTITY:-}" || "$ULLAGE_MAC_SIGN_IDENTITY" == "-" ]]; then
        echo "ULLAGE_MAC_SIGN_IDENTITY is required for $action" >&2
        exit 2
    fi
    if [[ "$ULLAGE_MAC_SIGN_IDENTITY" == *$'\n'* || "$ULLAGE_MAC_SIGN_IDENTITY" == *$'\r'* ]]; then
        echo "ULLAGE_MAC_SIGN_IDENTITY must not contain newlines" >&2
        exit 2
    fi
    if [[ "$action" == "notarize" && -z "${ULLAGE_MAC_NOTARY_PROFILE:-}" ]]; then
        echo "ULLAGE_MAC_NOTARY_PROFILE is required for notarize" >&2
        exit 2
    fi
    if [[ "${ULLAGE_MAC_NOTARY_PROFILE:-}" == *$'\n'* || "${ULLAGE_MAC_NOTARY_PROFILE:-}" == *$'\r'* ]]; then
        echo "ULLAGE_MAC_NOTARY_PROFILE must not contain newlines" >&2
        exit 2
    fi
    timeout="${ULLAGE_MAC_SIGN_TIMEOUT:-1800}"
    if [[ ! "$timeout" =~ ^[1-9][0-9]*$ ]]; then
        echo "ULLAGE_MAC_SIGN_TIMEOUT must be a positive integer" >&2
        exit 2
    fi

    tmux_bin="$(remote_exec /bin/bash --norc -c 'command -v tmux || { for candidate in /opt/homebrew/bin/tmux /usr/local/bin/tmux; do [[ -x "$candidate" ]] && { printf "%s\n" "$candidate"; exit 0; }; done; exit 127; }')" || {
        echo "tmux is not available on the remote Mac" >&2
        exit 1
    }
    if [[ -z "$tmux_bin" || "$tmux_bin" == *$'\n'* || "$tmux_bin" != /* ]]; then
        echo "the remote tmux path is invalid" >&2
        exit 1
    fi
    if ! remote_exec "$tmux_bin" has-session -t "=$ULLAGE_MAC_GUI_TMUX_SESSION" >/dev/null 2>&1; then
        echo "remote GUI tmux session does not exist: $ULLAGE_MAC_GUI_TMUX_SESSION" >&2
        exit 1
    fi
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package_dir="$(cd "$script_dir/.." && pwd)"
repository_dir="$(git -C "$package_dir" rev-parse --show-toplevel)"
branch="$(git -C "$repository_dir" branch --show-current)"
if [[ -z "$branch" || ! "$branch" =~ ^[A-Za-z0-9._/-]+$ ]]; then
    echo "the current Git branch is not safe for a remote path" >&2
    exit 2
fi

remote_dir="ullage-build/$branch"
ssh "$ULLAGE_MAC_SSH" "mkdir -p ~/$remote_dir"
rsync -a --delete --exclude .build --exclude build "$package_dir/" "$ULLAGE_MAC_SSH:~/$remote_dir/"

if [[ "$action" == "sign" || "$action" == "notarize" ]]; then
    remote_home="$(remote_exec /bin/bash --norc -c 'printf "%s\n" "$HOME"')"
    if [[ -z "$remote_home" || "$remote_home" == *$'\n'* || "$remote_home" != /* ]]; then
        echo "the remote home directory is invalid" >&2
        exit 1
    fi
    remote_package_dir="$remote_home/$remote_dir"
    run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM"
    window_name="ullage-$action-$run_id"
    log_file="$remote_package_dir/.ullage-$action-$run_id.log"
    status_file="$remote_package_dir/.ullage-$action-$run_id.status"
    launcher="$remote_package_dir/scripts/remote.sh"
    launcher_command="$(shell_join /bin/bash "$launcher" __signed-launcher \
        "$remote_package_dir" "$action" "$ULLAGE_MAC_SIGN_IDENTITY" \
        "${ULLAGE_MAC_NOTARY_PROFILE:-}" "$log_file" "$status_file")"

    remote_exec "$tmux_bin" new-window -t "=$ULLAGE_MAC_GUI_TMUX_SESSION" \
        -n "$window_name" -d -- /bin/bash -c "$launcher_command"
    cleanup_window() {
        remote_exec "$tmux_bin" kill-window \
            -t "=$ULLAGE_MAC_GUI_TMUX_SESSION:=$window_name" >/dev/null 2>&1 || true
    }
    trap cleanup_window EXIT

    deadline=$((SECONDS + timeout))
    while ! remote_exec /bin/test -f "$status_file" >/dev/null 2>&1; do
        if ! remote_exec "$tmux_bin" list-panes \
            -t "=$ULLAGE_MAC_GUI_TMUX_SESSION:=$window_name" >/dev/null 2>&1; then
            remote_exec /bin/cat "$log_file" 2>/dev/null || true
            echo "the remote $action launcher exited without publishing a status" >&2
            exit 1
        fi
        if (( SECONDS >= deadline )); then
            remote_exec /bin/cat "$log_file" 2>/dev/null || true
            echo "$action timed out after $timeout seconds" >&2
            exit 124
        fi
        sleep 2
    done
    status="$(remote_exec /bin/cat "$status_file")"
    remote_exec /bin/cat "$log_file"
    if [[ ! "$status" =~ ^([0-9]|[1-9][0-9]|1[0-9][0-9]|2[0-4][0-9]|25[0-5])$ ]]; then
        echo "the remote launcher wrote an invalid exit status" >&2
        exit 1
    fi
    if [[ "$status" -ne 0 ]]; then
        exit "$status"
    fi
    if [[ "$action" == "notarize" ]]; then
        version="$(make -C "$package_dir" --no-print-directory -s print-version)"
        mkdir -p "$package_dir/build"
        rsync -a "$ULLAGE_MAC_SSH:$remote_package_dir/build/Ullage-$version.zip" "$package_dir/build/"
        echo "archive: $package_dir/build/Ullage-$version.zip"
    fi
elif [[ "$action" == "build" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift build -c release --arch arm64"
elif [[ "$action" == "test" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift test"
elif [[ "$action" == "bundle" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && make bundle"
else
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift build -c release --arch arm64"
    ssh "$ULLAGE_MAC_SSH" "pkill -x UllageMac >/dev/null 2>&1 || true; cd ~/$remote_dir && mkdir -p build && nohup .build/release/UllageMac --mock >build/ullage-mac.log 2>&1 &"
    timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
    remote_shot="$remote_dir/shots/$timestamp.png"
    if ssh "$ULLAGE_MAC_SSH" "mkdir -p ~/$remote_dir/shots && screencapture -x ~/$remote_shot"; then
        mkdir -p "$package_dir/build/shots"
        rsync -a "$ULLAGE_MAC_SSH:~/$remote_shot" "$package_dir/build/shots/"
        echo "screenshot: $package_dir/build/shots/$timestamp.png"
    else
        echo "warning: screencapture failed; the application is still running" >&2
    fi
fi
