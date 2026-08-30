.PHONY: build bundle run clean

VERSION ?= 0.1.0
APP := build/Ullage.app
CONTENTS := $(APP)/Contents

build:
	swift build -c release --arch arm64

bundle: build
	@printf '%s\n' "$(VERSION)" | grep -Eq '^[0-9]+(\.[0-9]+)*$$' || \
		{ echo "VERSION must be dot-separated integers" >&2; exit 2; }
	@set -eu; \
	bin_dir="$$(swift build -c release --arch arm64 --show-bin-path)"; \
	resource_bundle="$$(find "$$bin_dir" -maxdepth 1 -type d -name '*UllageKit*.bundle' -print -quit)"; \
	test -n "$$resource_bundle" || { echo "UllageKit resource bundle not found in $$bin_dir" >&2; exit 1; }; \
	rm -rf "$(APP)"; \
	mkdir -p "$(CONTENTS)/MacOS" "$(CONTENTS)/Resources"; \
	sed 's/@VERSION@/$(VERSION)/g' Resources/Info.plist > "$(CONTENTS)/Info.plist"; \
	cp "$$bin_dir/UllageMac" "$(CONTENTS)/MacOS/UllageMac"; \
	cp -R "$$resource_bundle" "$(CONTENTS)/Resources/"; \
	printf 'APPL????' > "$(CONTENTS)/PkgInfo"; \
	codesign --force --sign - --timestamp=none "$(APP)"

run: bundle
	open "$(APP)"

clean:
	rm -rf .build build
