#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <app> <team-id> <sign-identity>" >&2
    exit 2
fi

app="$1"
expected_team="$2"
sign_identity="$3"
helper="$app/Contents/Library/LoginItems/UllageDaemonHelper.app"
tool="$helper/Contents/Resources/ullage-daemon"
temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/ullage-signatures.XXXXXX")"
trap 'rm -rf "$temporary_dir"' EXIT

fail() {
    echo "signature verification failed: $1" >&2
    exit 1
}

metadata_value() {
    local path="$1" key="$2"
    codesign -dv --verbose=4 "$path" 2>&1 \
        | awk -F= -v key="$key" '$1 == key && !found { sub(/^[^=]*=/, ""); print; found = 1 }'
}

extract_entitlements() {
    local path="$1" output="$2"
    codesign -d --entitlements :- "$path" >"$output" 2>/dev/null
    /usr/bin/plutil -lint "$output" >/dev/null
}

entitlement_value() {
    local plist="$1" key="$2"
    /usr/libexec/PlistBuddy -c "Print :$key" "$plist" 2>/dev/null
}

assert_value() {
    local actual="$1" expected="$2" label="$3"
    [[ "$actual" == "$expected" ]] || fail "$label is '$actual', expected '$expected'"
}

assert_entitlement_keys() {
    local plist="$1" expected="$2" label="$3" count
    count="$(awk '{ total += gsub(/<key>/, "") } END { print total + 0 }' "$plist")"
    assert_value "$count" "$expected" "$label entitlement count"
}

assert_group() {
    local plist="$1" label="$2" groups
    groups="$(entitlement_value "$plist" com.apple.security.application-groups \
        | tr -d '[:space:]')"
    assert_value "$groups" 'Array{group.com.ullage.mac}' "$label App Group"
}

assert_requirement() {
    local path="$1" identifier="$2" requirement
    requirement="$(codesign -d -r- "$path" 2>&1)"
    grep -Fq "identifier \"$identifier\"" <<<"$requirement" \
        || fail "$identifier designated requirement does not bind its identifier"
    if [[ "$sign_identity" != "-" ]]; then
        grep -Fq "certificate leaf[subject.OU] = \"$expected_team\"" <<<"$requirement" \
            || fail "$identifier designated requirement does not bind team $expected_team"
    fi
}

for path in "$tool" "$helper" "$app"; do
    codesign --verify --strict "$path"
done
codesign --verify --deep --strict "$app"

app_entitlements="$temporary_dir/app.plist"
helper_entitlements="$temporary_dir/helper.plist"
tool_entitlements="$temporary_dir/tool.plist"
extract_entitlements "$app" "$app_entitlements"
extract_entitlements "$helper" "$helper_entitlements"
extract_entitlements "$tool" "$tool_entitlements"

assert_value "$(metadata_value "$app" Identifier)" com.ullage.mac "main app identifier"
assert_value "$(metadata_value "$helper" Identifier)" com.ullage.mac.daemon "helper identifier"
assert_value "$(metadata_value "$tool" Identifier)" com.ullage.mac.daemon.tool "tool identifier"

assert_value "$(entitlement_value "$app_entitlements" com.apple.security.app-sandbox)" true \
    "main app sandbox"
assert_value "$(entitlement_value "$app_entitlements" com.apple.security.network.client)" true \
    "main app outbound network"
assert_value "$(entitlement_value "$app_entitlements" com.apple.security.network.server)" true \
    "main app inbound network"
assert_group "$app_entitlements" "main app"

assert_value "$(entitlement_value "$helper_entitlements" com.apple.security.app-sandbox)" true \
    "helper sandbox"
assert_value "$(entitlement_value "$helper_entitlements" com.apple.security.network.client)" true \
    "helper outbound network"
assert_value "$(entitlement_value "$helper_entitlements" com.apple.security.network.server)" true \
    "helper inbound network"
assert_group "$helper_entitlements" "helper"

assert_value "$(entitlement_value "$tool_entitlements" com.apple.security.app-sandbox)" true \
    "tool sandbox"
assert_value "$(entitlement_value "$tool_entitlements" com.apple.security.inherit)" true \
    "tool sandbox inheritance"
assert_entitlement_keys "$tool_entitlements" 2 "tool"

if [[ "$sign_identity" == "-" ]]; then
    assert_entitlement_keys "$app_entitlements" 4 "main app"
    assert_entitlement_keys "$helper_entitlements" 4 "helper"
else
    [[ -f "$app/Contents/embedded.provisionprofile" ]] \
        || fail "main app provisioning profile is missing"
    [[ -f "$helper/Contents/embedded.provisionprofile" ]] \
        || fail "helper provisioning profile is missing"
    for path in "$app" "$helper" "$tool"; do
        assert_value "$(metadata_value "$path" TeamIdentifier)" "$expected_team" \
            "$(metadata_value "$path" Identifier) team"
        authority="$(metadata_value "$path" Authority)"
        [[ "$authority" == 'Developer ID Application:'* ]] \
            || fail "$(metadata_value "$path" Identifier) is not Developer ID signed"
    done
    assert_requirement "$app" com.ullage.mac
    assert_requirement "$helper" com.ullage.mac.daemon
    assert_requirement "$tool" com.ullage.mac.daemon.tool
    assert_value "$(entitlement_value "$app_entitlements" com.apple.application-identifier)" \
        "$expected_team.com.ullage.mac" "main app application identifier"
    assert_value "$(entitlement_value "$helper_entitlements" com.apple.application-identifier)" \
        "$expected_team.com.ullage.mac.daemon" "helper application identifier"
    assert_value "$(entitlement_value "$app_entitlements" com.apple.developer.team-identifier)" \
        "$expected_team" "main app entitlement team"
    assert_value "$(entitlement_value "$helper_entitlements" com.apple.developer.team-identifier)" \
        "$expected_team" "helper entitlement team"
    assert_entitlement_keys "$app_entitlements" 6 "main app"
    assert_entitlement_keys "$helper_entitlements" 6 "helper"
fi

echo "Verified nested signatures, identifiers, designated requirements, teams, and entitlements."
