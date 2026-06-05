.PHONY: build install test bench clean

build:
	cargo build --release
	cp target/release/pull-all bin/pull-all

install: build
	cp target/release/pull-all $(HOME)/bin/pull-all
	mkdir -p $(HOME)/bin/pull-all-siblings
	cp pull-all-siblings/pull-all-repos $(HOME)/bin/pull-all-siblings/pull-all-repos

test:
	cargo test

bench:
	@echo "Running benchmark on current directory (use --timeout 5 for quick mode)..."
	time bin/pull-all --no-tui 2>&1

clean:
	cargo clean
	rm -f bin/pull-all
