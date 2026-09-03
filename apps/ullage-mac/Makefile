.PHONY: build rust-build bundle check-notarize-config notarize print-version run clean \
	archive export-app-store verify-signatures

VERSION ?= 0.1.0
SIGN_IDENTITY ?= -
NOTARY_PROFILE ?=
APP_PROFILE ?=
HELPER_PROFILE ?=
ICON_PALETTE ?= oxblood
export ULLAGE_MAKE_VERSION := $(value VERSION)
export ULLAGE_MAKE_SIGN_IDENTITY := $(value SIGN_IDENTITY)
export ULLAGE_MAKE_NOTARY_PROFILE := $(value NOTARY_PROFILE)
export ULLAGE_MAKE_APP_PROFILE := $(value APP_PROFILE)
export ULLAGE_MAKE_HELPER_PROFILE := $(value HELPER_PROFILE)
APP := build/Ullage.app
CONTENTS := $(APP)/Contents
HELPER := $(CONTENTS)/Library/LoginItems/UllageDaemonHelper.app
HELPER_CONTENTS := $(HELPER)/Contents
REPOSITORY := ../..
RUST_DAEMON := $(REPOSITORY)/target/release/ullage
NOTARY_UPLOAD := build/Ullage-notarization.zip
ARCHIVE := build/UllageMac.xcarchive
APP_STORE_EXPORT := build/app-store
DEVELOPMENT_TEAM ?= 73D5UVY728
APP_STORE_PROFILE ?=

build:
	swift build -c release --arch arm64

rust-build:
	cd "$(REPOSITORY)" && cargo build --locked --release -p ullage-app

bundle: build rust-build
	@printf '%s\n' "$${ULLAGE_MAKE_VERSION}" | grep -Eq '^[0-9]+(\.[0-9]+)*$$' || \
		{ echo "VERSION must be dot-separated integers" >&2; exit 2; }
	@set -eu; \
	bin_dir="$$(swift build -c release --arch arm64 --show-bin-path)"; \
	resource_bundle="$$(find "$$bin_dir" -maxdepth 1 -type d -name '*UllageKit*.bundle' -print -quit)"; \
	test -n "$$resource_bundle" || { echo "UllageKit resource bundle not found in $$bin_dir" >&2; exit 1; }; \
	iconset="$(CURDIR)/build/AppIcon.iconset"; \
	icns="$(CURDIR)/build/AppIcon.icns"; \
	rm -rf "$$iconset" "$$icns"; \
	"$$bin_dir/UllageMac" --render-iconset "$$iconset" --palette "$(ICON_PALETTE)"; \
	iconutil -c icns "$$iconset" -o "$$icns"; \
	rm -rf "$(APP)"; \
	mkdir -p "$(CONTENTS)/MacOS" "$(CONTENTS)/Resources" \
		"$(HELPER_CONTENTS)/MacOS" "$(HELPER_CONTENTS)/Resources"; \
	sed "s/@VERSION@/$${ULLAGE_MAKE_VERSION}/g" Resources/Info.plist > "$(CONTENTS)/Info.plist"; \
	sed "s/@VERSION@/$${ULLAGE_MAKE_VERSION}/g" Resources/UllageDaemonHelper-Info.plist \
		> "$(HELPER_CONTENTS)/Info.plist"; \
	cp "$$bin_dir/UllageMac" "$(CONTENTS)/MacOS/UllageMac"; \
	cp "$$bin_dir/UllageDaemonHelper" "$(HELPER_CONTENTS)/MacOS/UllageDaemonHelper"; \
	cp "$(RUST_DAEMON)" "$(HELPER_CONTENTS)/Resources/ullage-daemon"; \
	cp "$$icns" "$(CONTENTS)/Resources/AppIcon.icns"; \
	cp -R "$$resource_bundle" "$(CONTENTS)/Resources/"; \
	printf 'APPL????' > "$(CONTENTS)/PkgInfo"; \
	if [ "$${ULLAGE_MAKE_SIGN_IDENTITY}" = "-" ]; then \
		codesign --force --sign - --timestamp=none \
			--identifier com.ullage.mac.daemon.tool \
			--entitlements Resources/UllageDaemon.entitlements \
			"$(HELPER_CONTENTS)/Resources/ullage-daemon"; \
		codesign --force --sign - --timestamp=none \
			--entitlements Resources/UllageDaemonHelper.entitlements "$(HELPER)"; \
		codesign --force --sign - --timestamp=none \
			--entitlements Resources/UllageMac.entitlements "$(APP)"; \
	else \
		app_profile="$${ULLAGE_MAKE_APP_PROFILE}"; \
		helper_profile="$${ULLAGE_MAKE_HELPER_PROFILE}"; \
		test -f "$$app_profile" || { echo "APP_PROFILE must name the main app provisioning profile" >&2; exit 2; }; \
		test -f "$$helper_profile" || { echo "HELPER_PROFILE must name the Login Item provisioning profile" >&2; exit 2; }; \
		app_profile_plist="$(CURDIR)/build/app-profile.plist"; \
		helper_profile_plist="$(CURDIR)/build/helper-profile.plist"; \
		security cms -D -i "$$app_profile" > "$$app_profile_plist"; \
		security cms -D -i "$$helper_profile" > "$$helper_profile_plist"; \
		app_identifier="$$(/usr/libexec/PlistBuddy -c 'Print :Entitlements:com.apple.application-identifier' "$$app_profile_plist")"; \
		helper_identifier="$$(/usr/libexec/PlistBuddy -c 'Print :Entitlements:com.apple.application-identifier' "$$helper_profile_plist")"; \
		test "$$app_identifier" = "$(DEVELOPMENT_TEAM).com.ullage.mac" || { echo "APP_PROFILE has the wrong application identifier" >&2; exit 2; }; \
		test "$$helper_identifier" = "$(DEVELOPMENT_TEAM).com.ullage.mac.daemon" || { echo "HELPER_PROFILE has the wrong application identifier" >&2; exit 2; }; \
		/usr/libexec/PlistBuddy -c 'Print :Entitlements:com.apple.security.application-groups' "$$app_profile_plist" | grep -Fq 'group.com.ullage.mac' || { echo "APP_PROFILE does not authorize group.com.ullage.mac" >&2; exit 2; }; \
		/usr/libexec/PlistBuddy -c 'Print :Entitlements:com.apple.security.application-groups' "$$helper_profile_plist" | grep -Fq 'group.com.ullage.mac' || { echo "HELPER_PROFILE does not authorize group.com.ullage.mac" >&2; exit 2; }; \
		cp "$$app_profile" "$(CONTENTS)/embedded.provisionprofile"; \
		cp "$$helper_profile" "$(HELPER_CONTENTS)/embedded.provisionprofile"; \
		app_entitlements="$(CURDIR)/build/UllageMac-distribution.entitlements"; \
		helper_entitlements="$(CURDIR)/build/UllageDaemonHelper-distribution.entitlements"; \
		cp Resources/UllageMac.entitlements "$$app_entitlements"; \
		cp Resources/UllageDaemonHelper.entitlements "$$helper_entitlements"; \
		/usr/libexec/PlistBuddy -c "Add :com.apple.application-identifier string $$app_identifier" "$$app_entitlements"; \
		/usr/libexec/PlistBuddy -c "Add :com.apple.developer.team-identifier string $(DEVELOPMENT_TEAM)" "$$app_entitlements"; \
		/usr/libexec/PlistBuddy -c "Add :com.apple.application-identifier string $$helper_identifier" "$$helper_entitlements"; \
		/usr/libexec/PlistBuddy -c "Add :com.apple.developer.team-identifier string $(DEVELOPMENT_TEAM)" "$$helper_entitlements"; \
		codesign --force --sign "$${ULLAGE_MAKE_SIGN_IDENTITY}" --options runtime --timestamp \
			--identifier com.ullage.mac.daemon.tool \
			--entitlements Resources/UllageDaemon.entitlements \
			"$(HELPER_CONTENTS)/Resources/ullage-daemon"; \
		codesign --force --sign "$${ULLAGE_MAKE_SIGN_IDENTITY}" --options runtime --timestamp \
			--entitlements "$$helper_entitlements" "$(HELPER)"; \
		codesign --force --sign "$${ULLAGE_MAKE_SIGN_IDENTITY}" --options runtime --timestamp \
			--entitlements "$$app_entitlements" "$(APP)"; \
	fi
	@$(MAKE) --no-print-directory verify-signatures

verify-signatures:
	codesign --verify --deep --strict "$(APP)"
	codesign -d --entitlements :- "$(APP)" >/dev/null
	codesign -d --entitlements :- "$(HELPER)" >/dev/null
	codesign -d --entitlements :- "$(HELPER_CONTENTS)/Resources/ullage-daemon" >/dev/null

check-notarize-config:
	@if [ -z "$${ULLAGE_MAKE_SIGN_IDENTITY}" ] || [ "$${ULLAGE_MAKE_SIGN_IDENTITY}" = "-" ]; then \
		echo "SIGN_IDENTITY must name a Developer ID Application identity" >&2; \
		exit 2; \
	fi
	@if [ -z "$${ULLAGE_MAKE_NOTARY_PROFILE}" ]; then \
		echo "NOTARY_PROFILE is required" >&2; \
		exit 2; \
	fi

notarize: check-notarize-config
	@$(MAKE) --no-print-directory bundle
	@set -eu; \
	distribution_zip="build/Ullage-$${ULLAGE_MAKE_VERSION}.zip"; \
	rm -f "$(NOTARY_UPLOAD)" "$$distribution_zip"; \
	ditto -c -k --keepParent "$(APP)" "$(NOTARY_UPLOAD)"; \
	xcrun notarytool submit "$(NOTARY_UPLOAD)" --keychain-profile "$${ULLAGE_MAKE_NOTARY_PROFILE}" --wait; \
	xcrun stapler staple "$(APP)"; \
	ditto -c -k --keepParent "$(APP)" "$$distribution_zip"; \
	rm -f "$(NOTARY_UPLOAD)"; \
		echo "Notarized archive: $$distribution_zip"

archive:
	xcodebuild archive -project UllageMac.xcodeproj -scheme UllageMacApp \
		-configuration Release -destination 'generic/platform=macOS' \
		-archivePath "$(ARCHIVE)" DEVELOPMENT_TEAM="$(DEVELOPMENT_TEAM)" \
		CODE_SIGN_STYLE=Automatic -allowProvisioningUpdates

export-app-store: archive
	@set -eu; \
	profile="$${ULLAGE_MAKE_APP_STORE_PROFILE:-$(APP_STORE_PROFILE)}"; \
	{ \
		printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>'; \
		printf '%s\n' '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'; \
		printf '%s\n' '<plist version="1.0"><dict>'; \
		printf '%s\n' '<key>method</key><string>app-store-connect</string>'; \
		printf '%s\n' '<key>signingStyle</key><string>automatic</string>'; \
		printf '%s\n' '<key>teamID</key><string>$(DEVELOPMENT_TEAM)</string>'; \
		printf '%s\n' '<key>uploadSymbols</key><true/>'; \
		if [ -n "$$profile" ]; then \
			printf '%s\n' '<key>provisioningProfiles</key><dict>'; \
			printf '<key>com.ullage.mac</key><string>%s</string>\n' "$$profile"; \
			printf '%s\n' '</dict>'; \
		fi; \
		printf '%s\n' '</dict></plist>'; \
	} > build/ExportOptions.plist
	rm -rf "$(APP_STORE_EXPORT)"
	xcodebuild -exportArchive -archivePath "$(ARCHIVE)" \
		-exportPath "$(APP_STORE_EXPORT)" -exportOptionsPlist build/ExportOptions.plist

print-version:
	@printf '%s\n' "$${ULLAGE_MAKE_VERSION}"

run: bundle
	open "$(APP)"

clean:
	rm -rf .build build
