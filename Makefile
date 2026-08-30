.PHONY: build mac clean

BUILD_DIR := build
BINARY := ullage

build:
	cargo build --release -p ullage-app
	mkdir -p $(BUILD_DIR)
	cp -f target/release/$(BINARY) $(BUILD_DIR)/$(BINARY)

mac:
	$(MAKE) -C apps/ullage-mac bundle

clean:
	rm -rf $(BUILD_DIR)
	cargo clean
