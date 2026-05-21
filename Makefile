.PHONY: build test bench clean

build:
	cargo build --release
	cp target/release/pull-all-tui bin/pull-all-tui

test:
	cargo test

bench:
	@echo "Running benchmark on current directory (use --timeout 5 for quick mode)..."
	time bin/pull-all-tui --no-tui 2>&1

clean:
	cargo clean
	rm -f bin/pull-all-tui
