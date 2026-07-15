.DEFAULT_GOAL := help

PNPM ?= pnpm
CARGO ?= cargo
TAURI_MANIFEST := src-tauri/Cargo.toml

.PHONY: help install dev dev-web preview build build-web check check-web check-rust test audit-web verify

help:
	@printf '%s\n' \
		'LAN Clipboard development commands:' \
		'  make install     Install frontend and Tauri CLI dependencies' \
		'  make dev         Start the Tauri development application' \
		'  make dev-web     Start only the Vite frontend' \
		'  make preview     Preview the built frontend' \
		'  make build       Build the Tauri release bundles' \
		'  make build-web   Build only the frontend' \
		'  make check       Run frontend build, Rust fmt and strict clippy' \
		'  make test        Run the Rust test suite' \
		'  make audit-web   Audit frontend dependencies via the npm registry' \
		'  make verify      Run check, test and dependency audit'

install:
	$(PNPM) install --frozen-lockfile

dev:
	$(PNPM) tauri dev

dev-web:
	$(PNPM) dev

preview:
	$(PNPM) preview

build:
	$(PNPM) tauri build -- --locked

build-web:
	$(PNPM) build

check: check-web check-rust

check-web:
	$(PNPM) build

check-rust:
	$(CARGO) fmt --manifest-path $(TAURI_MANIFEST) --all -- --check
	$(CARGO) clippy --manifest-path $(TAURI_MANIFEST) --locked --all-targets --all-features -- -D warnings

test:
	$(CARGO) test --manifest-path $(TAURI_MANIFEST) --locked --all-targets --all-features

audit-web:
	$(PNPM) audit --registry=https://registry.npmjs.org --audit-level moderate

verify: check test audit-web
