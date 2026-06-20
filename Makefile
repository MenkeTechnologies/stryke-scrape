SHELL := /bin/sh
.PHONY: all build debug release test clean install help

all: release

help:
	@printf '%s\n' \
	  'targets:' \
	  '  make release   - cargo build --release' \
	  '  make debug     - cargo build' \
	  '  make test      - cargo test then `s test t/`  (live crawl opt-in via $$SCRAPE_LIVE_URL)' \
	  '  make install   - `s pkg install -g .` (cdylib lands in ~/.stryke/store/scrape@<ver>/)' \
	  '  make clean     - cargo clean'

release:
	cargo build --release

debug build:
	cargo build

test:
	cargo test
	s test t/ || true

install: release
	s pkg install -g .

clean:
	cargo clean
