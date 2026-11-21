.PHONY: fmt lint build release

fmt:
	@cargo fmt

lint:
	@cargo clippy --all-features

build:
	@cargo build

release:
	@cargo build --release
