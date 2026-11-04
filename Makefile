# The version is kept in the readme, so pull it out of there instead of
# maintaining a second copy here.
VERSION := $(shell sed -n 's/^v\([0-9.]*\)$$/\1/p' readme.md | head -n 1)

.DEFAULT_GOAL := all
.PHONY: all build build-release test install clean

all: ## print the ascii header, the version, and list the commands
	@awk 'BEGIN{f=0} /^```/ { if (f) { f=0; exit } f=1; next } f && !/^```/ { print }' readme.md
	@echo ""
	@echo "commands:"
	@echo "  make build         build the debug binary"
	@echo "  make build-release build the release binary"
	@echo "  make test          run the test suite"
	@echo "  make install       install the binary"

build: ## build the debug binary
	cargo build

build-release: ## build the release binary
	cargo build --release

test: ## run the test suite
	cargo test --locked --all-targets

install: ## install the binary
	cargo install --path .

clean: ## clean the build artifacts
	cargo clean
