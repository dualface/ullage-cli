#!/usr/bin/env bash
# Rewrite Formula/ullage.rb in dualface/homebrew-tap from a GitHub Release.
#
#   scripts/sync-homebrew-tap.sh v0.1.1
#
# The Release workflow runs this after publishing archives. TAP_TOKEN must be
# a PAT with contents:write on dualface/homebrew-tap; GITHUB_TOKEN cannot push
# to another repository. Without that secret, repair the formula by hand:
#
#   scripts/sync-homebrew-tap.sh v0.1.1

set -euo pipefail

REPO_SLUG=dualface/ullage-cli
TAP_SLUG=dualface/homebrew-tap
# Archives Homebrew installs from; the workflow also publishes a Windows zip.
TAP_TARGETS=(
	aarch64-apple-darwin
	x86_64-apple-darwin
	aarch64-unknown-linux-gnu
	x86_64-unknown-linux-gnu
)

skip_wait=0
dry_run=0
checksums_file=

usage() {
	cat >&2 <<EOF
usage: scripts/sync-homebrew-tap.sh [options] <version>

  <version>      release tag, e.g. v0.1.1

options:
  --skip-wait    do not wait for checksums.txt on the GitHub Release
  --dry-run      print the formula; do not clone or push the tap
  --checksums F  read checksums from local file F instead of the release
  -h, --help     show this message
EOF
	exit 2
}

die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

step() {
	printf '\n==> %s\n' "$*" >&2
}

version=
while [ $# -gt 0 ]; do
	case "$1" in
	--skip-wait) skip_wait=1 ;;
	--dry-run) dry_run=1 ;;
	--checksums)
		shift
		[ $# -gt 0 ] || die "--checksums needs a file path"
		checksums_file="$1"
		;;
	-h | --help) usage ;;
	-*) die "unknown option: $1" ;;
	*)
		[ -n "$version" ] && die "unexpected extra argument: $1"
		version="$1"
		;;
	esac
	shift
done

[ -n "$version" ] || usage

if ! printf '%s' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
	die "version must look like v0.1.1, got: $version"
fi
bare_version="${version#v}"

for tool in git awk; do
	command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done
if [ -z "$checksums_file" ]; then
	command -v gh >/dev/null 2>&1 || die "missing required tool: gh"
	gh auth status >/dev/null 2>&1 || die "gh is not authenticated; run: gh auth login"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$repo_root"

workdir=
cleanup() {
	[ -n "$workdir" ] && [ -d "$workdir" ] && rm -rf "$workdir"
}
trap cleanup EXIT

workdir="$(mktemp -d)"

if [ -n "$checksums_file" ]; then
	[ -f "$checksums_file" ] || die "no such checksums file: $checksums_file"
	cp "$checksums_file" "$workdir/checksums.txt"
elif [ "$skip_wait" -eq 1 ]; then
	step "reading checksums.txt from $version"
	gh release download "$version" --repo "$REPO_SLUG" --pattern checksums.txt --dir "$workdir"
else
	step "waiting for release assets"
	deadline=$(($(date +%s) + 1200))
	while :; do
		if gh release view "$version" --repo "$REPO_SLUG" --json assets \
			--jq '[.assets[].name] | index("checksums.txt")' 2>/dev/null | grep -q '^[0-9]'; then
			printf 'release %s is published with checksums.txt\n' "$version" >&2
			break
		fi
		if [ "$(date +%s)" -ge "$deadline" ]; then
			die "timed out waiting for $version; check: gh run list --repo $REPO_SLUG"
		fi
		printf '  still building, retrying in 15s...\n' >&2
		sleep 15
	done
	gh release download "$version" --repo "$REPO_SLUG" --pattern checksums.txt --dir "$workdir"
fi

# checksums.txt is `sha256sum -- *` output: "<hash>  <filename>".
sha_for() {
	local name="$1" sha
	sha="$(awk -v want="$name" '
		$2 == want || $2 == "*" want {
			if (found++) exit 2
			value = $1
		}
		END {
			if (found == 1) print value
			else exit 1
		}' "$workdir/checksums.txt")" || die "expected exactly one checksum for $name in checksums.txt"
	[[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "invalid checksum for $name in checksums.txt"
	printf '%s' "$sha"
}

sha_aarch64_apple_darwin=
sha_x86_64_apple_darwin=
sha_aarch64_unknown_linux_gnu=
sha_x86_64_unknown_linux_gnu=
for target in "${TAP_TARGETS[@]}"; do
	name="ullage-${target}.tar.gz"
	sha="$(sha_for "$name")"
	eval "sha_$(printf '%s' "$target" | tr - _)=\$sha"
	printf '  %s  %s\n' "$name" "$sha" >&2
done

asset_url() {
	printf 'https://github.com/%s/releases/download/%s/ullage-%s.tar.gz' "$REPO_SLUG" "$version" "$1"
}

formula="$(
	cat <<EOF
class Ullage < Formula
  desc "Local daemon and CLI for AI subscription usage"
  homepage "https://github.com/${REPO_SLUG}"
  version "${bare_version}"
  license "MIT"

  on_macos do
    on_arm do
      url "$(asset_url aarch64-apple-darwin)"
      sha256 "${sha_aarch64_apple_darwin}"
    end
    on_intel do
      url "$(asset_url x86_64-apple-darwin)"
      sha256 "${sha_x86_64_apple_darwin}"
    end
  end

  on_linux do
    on_arm do
      url "$(asset_url aarch64-unknown-linux-gnu)"
      sha256 "${sha_aarch64_unknown_linux_gnu}"
    end
    on_intel do
      url "$(asset_url x86_64-unknown-linux-gnu)"
      sha256 "${sha_x86_64_unknown_linux_gnu}"
    end
  end

  def install
    bin.install "ullage"
  end

  def post_install
    # Stop first so an upgrade bootstraps the new Cellar keg. kickstart of a
    # still-loaded job would keep the previous ProgramArguments.
    ohai "Installing and starting the Ullage user daemon"
    quiet_system bin/"ullage", "daemon", "stop"
    unless quiet_system bin/"ullage", "daemon", "install"
      opoo "Could not install the user daemon. Run: ullage daemon install"
      return
    end
    return if quiet_system bin/"ullage", "daemon", "start"

    opoo "Could not start the user daemon. Run: ullage daemon start"
  end

  def caveats
    <<~EOS
      brew install and brew upgrade install and start the user-level daemon.
      After an upgrade they pin the new Cellar keg path. If that step was
      skipped, run:
        ullage daemon install
    EOS
  end

  test do
    assert_match "ullage #{version}", shell_output("#{bin}/ullage --version")
  end
end
EOF
)"

if [ "$dry_run" -eq 1 ]; then
	step "formula that would be written to $TAP_SLUG"
	printf '%s\n' "$formula"
	exit 0
fi

step "updating $TAP_SLUG"
gh repo clone "$TAP_SLUG" "$workdir/tap" -- --depth 1 --quiet
printf '%s\n' "$formula" >"$workdir/tap/Formula/ullage.rb"

if git -C "$workdir/tap" diff --quiet -- Formula/ullage.rb; then
	printf 'formula already matches %s; nothing to push\n' "$version" >&2
else
	git -C "$workdir/tap" add Formula/ullage.rb
	git -C "$workdir/tap" commit --quiet -m "ullage ${bare_version}"
	git -C "$workdir/tap" push --quiet origin HEAD:main
	printf 'pushed formula for %s to %s\n' "$bare_version" "$TAP_SLUG" >&2
fi

step "done"
cat >&2 <<EOF
release:  https://github.com/${REPO_SLUG}/releases/tag/${version}
formula:  https://github.com/${TAP_SLUG}/blob/main/Formula/ullage.rb
verify:   brew update && brew install dualface/tap/ullage
EOF
