.PHONY: build bundle check-notarize-config notarize print-version run clean

VERSION ?= 0.1.0
SIGN_IDENTITY ?= -
NOTARY_PROFILE ?=
ICON_PALETTE ?= oxblood
export ULLAGE_MAKE_VERSION := $(value VERSION)
export ULLAGE_MAKE_SIGN_IDENTITY := $(value SIGN_IDENTITY)
export ULLAGE_MAKE_NOTARY_PROFILE := $(value NOTARY_PROFILE)
APP := build/Ullage.app
CONTENTS := $(APP)/Contents
NOTARY_UPLOAD := build/Ullage-notarization.zip

build:
	swift build -c release --arch arm64

bundle: build
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
	mkdir -p "$(CONTENTS)/MacOS" "$(CONTENTS)/Resources"; \
	sed "s/@VERSION@/$${ULLAGE_MAKE_VERSION}/g" Resources/Info.plist > "$(CONTENTS)/Info.plist"; \
	cp "$$bin_dir/UllageMac" "$(CONTENTS)/MacOS/UllageMac"; \
	cp "$$icns" "$(CONTENTS)/Resources/AppIcon.icns"; \
	cp -R "$$resource_bundle" "$(CONTENTS)/Resources/"; \
	printf 'APPL????' > "$(CONTENTS)/PkgInfo"; \
	if [ "$${ULLAGE_MAKE_SIGN_IDENTITY}" = "-" ]; then \
		codesign --force --sign - --timestamp=none "$(APP)"; \
	else \
		codesign --force --sign "$${ULLAGE_MAKE_SIGN_IDENTITY}" --options runtime --timestamp "$(APP)"; \
	fi

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

print-version:
	@printf '%s\n' "$${ULLAGE_MAKE_VERSION}"

run: bundle
	open "$(APP)"

clean:
	rm -rf .build build
