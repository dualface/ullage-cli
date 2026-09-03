#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "__signed-launcher" ]]; then
    if [[ $# -ne 10 ]]; then
        echo "invalid signed launcher invocation" >&2
        exit 2
    fi
    package_dir="$2"
    action="$3"
    identity="$4"
    profile="$5"
    log_file="$6"
    status_file="$7"
    version="$8"
    app_profile="$9"
    helper_profile="${10}"
    if [[ "$action" != "sign" && "$action" != "notarize" ]]; then
        echo "invalid signed launcher action" >&2
        exit 2
    fi
    export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
    cd "$package_dir"
    set +e
    if [[ "$action" == "sign" ]]; then
        make bundle SIGN_IDENTITY="$identity" APP_PROFILE="$app_profile" \
            HELPER_PROFILE="$helper_profile" VERSION="$version" >"$log_file" 2>&1
    else
        make notarize SIGN_IDENTITY="$identity" NOTARY_PROFILE="$profile" \
            APP_PROFILE="$app_profile" HELPER_PROFILE="$helper_profile" \
            VERSION="$version" >"$log_file" 2>&1
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

remote_exec_until() {
    local deadline="$1"
    shift
    local command wrapped ssh_pid ssh_status
    (( SECONDS < deadline )) || return 124
    command="$(shell_join "$@")"
    wrapped="$(shell_join /bin/bash --norc -c "$command")"
    ssh "$ULLAGE_MAC_SSH" "$wrapped" &
    ssh_pid=$!
    while kill -0 "$ssh_pid" >/dev/null 2>&1; do
        if (( SECONDS >= deadline )); then
            kill -TERM "$ssh_pid" >/dev/null 2>&1 || true
            kill -KILL "$ssh_pid" >/dev/null 2>&1 || true
            wait "$ssh_pid" 2>/dev/null || true
            return 124
        fi
        sleep 1
    done
    if wait "$ssh_pid"; then
        ssh_status=0
    else
        ssh_status=$?
    fi
    if (( SECONDS >= deadline )); then
        return 124
    fi
    return "$ssh_status"
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
    if [[ -z "${ULLAGE_MAC_APP_PROFILE:-}" || -z "${ULLAGE_MAC_HELPER_PROFILE:-}" ]]; then
        echo "ULLAGE_MAC_APP_PROFILE and ULLAGE_MAC_HELPER_PROFILE are required for $action" >&2
        exit 2
    fi
    if [[ "$ULLAGE_MAC_APP_PROFILE" == *$'\n'* || "$ULLAGE_MAC_APP_PROFILE" == *$'\r'* ||
          "$ULLAGE_MAC_HELPER_PROFILE" == *$'\n'* || "$ULLAGE_MAC_HELPER_PROFILE" == *$'\r'* ]]; then
        echo "provisioning profile paths must not contain newlines" >&2
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

# Mark this build directory as in use for the lifetime of the run so a
# concurrent run for another branch does not prune it. The marker holds the
# epoch time; a marker older than two hours is treated as a crashed leftover.
in_progress_marker="\$HOME/$remote_dir/.in-progress"
cleanup_marker() {
    remote_exec /bin/bash --norc -c "rm -f -- $in_progress_marker" >/dev/null 2>&1 || true
}
trap cleanup_marker EXIT
remote_exec /bin/bash --norc -c "printf '%s\\n' \"\$(date +%s)\" > $in_progress_marker"

rsync -a --delete \
    --exclude .git --exclude .memsearch --exclude .onevoke --exclude kanban \
    --exclude worktrees --exclude target --exclude .build --exclude build \
    --exclude .in-progress \
    "$repository_dir/" "$ULLAGE_MAC_SSH:~/$remote_dir/"

# Keep only the build directory for this branch: every other direct child of
# ~/ullage-build is removed, except sibling builds whose marker is still fresh.
# Symlinks are removed as links and never followed. A branch name containing
# a slash nests below ~/ullage-build, so pruning is skipped rather than guessed.
if [[ "$branch" == */* ]]; then
    echo "warning: branch name contains '/'; skipping remote build cleanup" >&2
else
    remote_exec /bin/bash --norc -c '
set -euo pipefail
keep="$1"
root="$HOME/ullage-build"
now="$(date +%s)"
cd "$root" || exit 0
shopt -s dotglob nullglob
for entry in *; do
    [[ "$entry" == "$keep" ]] && continue
    if [[ -d "$entry" && ! -L "$entry" && -f "$entry/.in-progress" ]]; then
        stamp="$(cat "$entry/.in-progress" 2>/dev/null || true)"
        [[ "$stamp" =~ ^[0-9]+$ ]] || stamp=0
        if (( now - stamp < 7200 )); then
            echo "keeping in-progress remote build: $entry" >&2
            continue
        fi
    fi
    rm -rf -- "$entry"
done
' prune "$branch"
fi

remote_home="$(remote_exec /bin/bash --norc -c 'printf "%s\n" "$HOME"')"
if [[ -z "$remote_home" || "$remote_home" == *$'\n'* || "$remote_home" != /* ]]; then
    echo "the remote home directory is invalid" >&2
    exit 1
fi
remote_repository_dir="$remote_home/$remote_dir"
remote_package_dir="$remote_repository_dir/apps/ullage-mac"

if [[ "$action" == "sign" || "$action" == "notarize" ]]; then
    if ! remote_exec /usr/bin/test -f "$ULLAGE_MAC_APP_PROFILE"; then
        echo "remote main app provisioning profile does not exist: $ULLAGE_MAC_APP_PROFILE" >&2
        exit 2
    fi
    if ! remote_exec /usr/bin/test -f "$ULLAGE_MAC_HELPER_PROFILE"; then
        echo "remote Login Item provisioning profile does not exist: $ULLAGE_MAC_HELPER_PROFILE" >&2
        exit 2
    fi
fi

if ! remote_exec /bin/test -x "$remote_home/.cargo/bin/rustup" >/dev/null 2>&1; then
    remote_exec /usr/bin/curl --proto '=https' --tlsv1.2 -sSf \
        https://sh.rustup.rs -o "$remote_repository_dir/rustup-init.sh"
    remote_exec /bin/sh "$remote_repository_dir/rustup-init.sh" -y --profile minimal \
        --default-toolchain none
    remote_exec /bin/rm -f "$remote_repository_dir/rustup-init.sh"
fi
if ! remote_exec "$remote_home/.cargo/bin/rustup" toolchain list \
    | /usr/bin/grep -Eq '^1\.85\.1-aarch64-apple-darwin( |$)'; then
    remote_exec "$remote_home/.cargo/bin/rustup" toolchain install 1.85.1 --profile minimal \
        --component rustfmt --component clippy
fi

if [[ "$action" == "sign" || "$action" == "notarize" ]]; then
    version="$(make -C "$package_dir" --no-print-directory -s print-version)"
    if [[ ! "$version" =~ ^[0-9]+(\.[0-9]+)*$ ]]; then
        echo "VERSION must be dot-separated integers" >&2
        exit 2
    fi
    run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM"
    window_name="ullage-$action-$run_id"
    log_file="$remote_package_dir/.ullage-$action-$run_id.log"
    status_file="$remote_package_dir/.ullage-$action-$run_id.status"
    launcher="$remote_package_dir/scripts/remote.sh"
    launcher_command="$(shell_join /bin/bash "$launcher" __signed-launcher \
        "$remote_package_dir" "$action" "$ULLAGE_MAC_SIGN_IDENTITY" \
        "${ULLAGE_MAC_NOTARY_PROFILE:-}" "$log_file" "$status_file" "$version" \
        "$ULLAGE_MAC_APP_PROFILE" "$ULLAGE_MAC_HELPER_PROFILE")"

    cleanup_window() {
        local cleanup_deadline=$((SECONDS + 10))
        remote_exec_until "$cleanup_deadline" "$tmux_bin" kill-window \
            -t "=$ULLAGE_MAC_GUI_TMUX_SESSION:=$window_name" >/dev/null 2>&1 || true
    }
    trap 'cleanup_window; cleanup_marker' EXIT
    remote_exec "$tmux_bin" new-window -t "=$ULLAGE_MAC_GUI_TMUX_SESSION" \
        -n "$window_name" -d -- /bin/bash -c "$launcher_command"

    deadline=$((SECONDS + timeout))
    while true; do
        if remote_exec_until "$deadline" /bin/test -f "$status_file" >/dev/null 2>&1; then
            break
        else
            probe_status=$?
        fi
        if [[ "$probe_status" -eq 124 ]]; then
            remote_exec_until "$((SECONDS + 10))" /bin/cat "$log_file" 2>/dev/null || true
            echo "$action timed out after $timeout seconds" >&2
            exit 124
        fi
        if remote_exec_until "$deadline" "$tmux_bin" list-panes \
            -t "=$ULLAGE_MAC_GUI_TMUX_SESSION:=$window_name" >/dev/null 2>&1; then
            :
        else
            probe_status=$?
            if [[ "$probe_status" -eq 124 ]]; then
                remote_exec_until "$((SECONDS + 10))" /bin/cat "$log_file" 2>/dev/null || true
                echo "$action timed out after $timeout seconds" >&2
                exit 124
            fi
            if remote_exec_until "$deadline" /bin/test -f "$status_file" >/dev/null 2>&1; then
                break
            fi
            probe_status=$?
            if [[ "$probe_status" -eq 124 ]]; then
                remote_exec_until "$((SECONDS + 10))" /bin/cat "$log_file" 2>/dev/null || true
                echo "$action timed out after $timeout seconds" >&2
                exit 124
            fi
            remote_exec_until "$((SECONDS + 10))" /bin/cat "$log_file" 2>/dev/null || true
            echo "the remote $action launcher exited without publishing a status" >&2
            exit 1
        fi
        if (( SECONDS >= deadline )); then
            remote_exec_until "$((SECONDS + 10))" /bin/cat "$log_file" 2>/dev/null || true
            echo "$action timed out after $timeout seconds" >&2
            exit 124
        fi
        sleep 1
    done
    status="$(remote_exec_until "$((SECONDS + 10))" /bin/cat "$status_file")"
    remote_exec_until "$((SECONDS + 30))" /bin/cat "$log_file"
    if [[ ! "$status" =~ ^([0-9]|[1-9][0-9]|1[0-9][0-9]|2[0-4][0-9]|25[0-5])$ ]]; then
        echo "the remote launcher wrote an invalid exit status" >&2
        exit 1
    fi
    if [[ "$status" -ne 0 ]]; then
        exit "$status"
    fi
    if [[ "$action" == "notarize" ]]; then
        mkdir -p "$package_dir/build"
        rsync -a "$ULLAGE_MAC_SSH:$remote_package_dir/build/Ullage-$version.zip" "$package_dir/build/"
        remote_exec /bin/bash --norc -c '
old_ui="$1/Contents/MacOS/UllageMac"
if [[ -x "$old_ui" ]]; then
    "$old_ui" --local-service-test unregister >/dev/null 2>&1 &
    cleanup_pid=$!
    for _ in {1..50}; do
        if ! kill -0 "$cleanup_pid" >/dev/null 2>&1; then
            wait "$cleanup_pid" 2>/dev/null || true
            exit 0
        fi
        sleep 0.1
    done
    kill -TERM "$cleanup_pid" >/dev/null 2>&1 || true
    wait "$cleanup_pid" 2>/dev/null || true
fi
' cleanup "$remote_home/Desktop/Ullage.app"
        remote_exec /usr/bin/ditto "$remote_package_dir/build/Ullage.app" \
            "$remote_home/Desktop/Ullage.app"
        remote_exec /usr/bin/pkill -x Ullage >/dev/null 2>&1 || true
        remote_exec /usr/bin/pkill -x UllageMac >/dev/null 2>&1 || true
        remote_exec /usr/bin/open "$remote_home/Desktop/Ullage.app"
        echo "archive: $package_dir/build/Ullage-$version.zip"
        echo "application: $remote_home/Desktop/Ullage.app"
    fi
elif [[ "$action" == "build" ]]; then
    remote_exec /bin/bash --norc -c \
        'export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; cd "$1" && make build rust-build' \
        build "$remote_package_dir"
elif [[ "$action" == "test" ]]; then
    remote_exec /bin/bash --norc -c \
        'export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; cd "$1" && swift test' \
        test "$remote_package_dir"
elif [[ "$action" == "bundle" ]]; then
    remote_exec /bin/bash --norc -c \
        'export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; cd "$1" && make bundle' \
        bundle "$remote_package_dir"
else
    remote_exec /bin/bash --norc -c \
        'cd "$1" && swift build -c release --arch arm64; pkill -x UllageMac >/dev/null 2>&1 || true; mkdir -p build; nohup .build/release/UllageMac --mock >build/ullage-mac.log 2>&1 &' \
        run "$remote_package_dir"
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
