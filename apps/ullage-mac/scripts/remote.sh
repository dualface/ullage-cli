#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${ULLAGE_MAC_SSH:-}" ]]; then
    echo "ULLAGE_MAC_SSH is required" >&2
    exit 2
fi

if [[ $# -ne 1 || ( "$1" != "test" && "$1" != "build" ) ]]; then
    echo "usage: $0 <test|build>" >&2
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
rsync -a --delete --exclude .build "$package_dir/" "$ULLAGE_MAC_SSH:~/$remote_dir/"

if [[ "$1" == "build" ]]; then
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift build -c release --arch arm64"
else
    ssh "$ULLAGE_MAC_SSH" "cd ~/$remote_dir && swift test"
fi
