.DEFAULT_GOAL := help

PNPM ?= pnpm
CARGO ?= cargo
TAURI_MANIFEST := src-tauri/Cargo.toml

.PHONY: help install dev dev-web preview build build-web check check-web check-rust test

help:
	@printf '%s\n' \
		'LAN Clipboard development commands:' \
		'  make install     Install frontend and Tauri CLI dependencies' \
		'  make dev         Start the Tauri development application' \
		'  make dev-web     Start only the Vite frontend' \
		'  make preview     Preview the built frontend' \
		'  make build       Build the Tauri release bundles' \
		'  make build-web   Build only the frontend' \
		'  make check       Run TypeScript and Rust compile checks' \
		'  make test        Run the Rust test suite'

install:
	$(PNPM) install

dev:
	$(PNPM) tauri dev

dev-web:
	$(PNPM) dev

preview:
	$(PNPM) preview

build:
	$(PNPM) tauri build

build-web:
	$(PNPM) build

check: check-web check-rust

check-web:
	$(PNPM) exec tsc --noEmit

check-rust:
	$(CARGO) check --manifest-path $(TAURI_MANIFEST)

test:
	$(CARGO) test --manifest-path $(TAURI_MANIFEST)
