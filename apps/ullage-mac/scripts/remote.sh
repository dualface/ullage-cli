#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${ULLAGE_MAC_SSH:-}" ]]; then
    echo "ULLAGE_MAC_SSH is required" >&2
    exit 2
fi

if [[ $# -ne 1 || ( "$1" != "test" && "$1" != "build" && "$1" != "bundle" && "$1" != "run" ) ]]; then
    echo "usage: $0 <test|build|bundle|run>" >&2
    exit 2
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

if [[ "$1" == "build" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift build -c release --arch arm64"
elif [[ "$1" == "test" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift test"
elif [[ "$1" == "bundle" ]]; then
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
