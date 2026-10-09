.DEFAULT_GOAL := help
SHELL := /usr/bin/env bash
# Падать на первой ошибке, на неопределённых переменных и внутри пайпов.
.SHELLFLAGS := -eu -o pipefail -c
.DELETE_ON_ERROR:
MAKEFLAGS += --warn-undefined-variables --no-builtin-rules

VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
COMMIT  ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo unknown)

# Мини-ПК: Ryzen AI HX 370, 12 ядер. Оставляем пару под систему.
JOBS ?= 10

# narsil-mcp — не --all-features: graph+frontend+neural+neural-onnx+wasm разом
# либо не собираются на native-таргете, либо тянут ort/tokenizers+oxigraph+
# rust-embed+wasm-bindgen одновременно и съедают диск. graph,neural — реальный
# боевой набор (см. cargo run в CLAUDE.md), проверен clippy-чистым 2026-08-31.
RUST_FEATURES ?= graph,neural

# Что индексирует `make rs-run`.
REPOS ?= ./test-fixtures

# Выгрузка 1С для стенда поиска (проприетарная, в репозиторий не копируется).
ONEC_REPO ?= $(HOME)/Downloads/ConfigFiles/CommonModules

.PHONY: help
help: ## Показать цели
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------- Go ---------
# В narsil-mcp нет Go-кода — эти цели не входят в `ci` ниже. Оставлены для
# переиспользования Makefile в других проектах (sage-wiki и т.п.).

.PHONY: go-lint
go-lint: ## golangci-lint по всему модулю
	golangci-lint run ./...

.PHONY: go-test
go-test: ## Тесты с детектором гонок, без кеша
	go test -race -count=1 -timeout=10m ./...

.PHONY: go-cover
go-cover: ## Покрытие с отчётом
	go test -race -count=1 -coverprofile=coverage.out -covermode=atomic ./...
	go tool cover -func=coverage.out | tail -n 1

.PHONY: go-vuln
go-vuln: ## Уязвимости, достижимые из нашего кода
	govulncheck ./...

.PHONY: go-dead
go-dead: ## Мёртвый код после агентских рефакторингов
	deadcode ./...

.PHONY: go-fuzz
go-fuzz: ## Короткий прогон всех фаззеров (30с на каждый)
	@for pkg in $$(go list ./...); do \
		for f in $$(go test -list='Fuzz.*' $$pkg 2>/dev/null | grep '^Fuzz' || true); do \
			echo ">>> $$pkg $$f"; \
			go test $$pkg -run=$$f -fuzz=$$f -fuzztime=30s || exit 1; \
		done; \
	done

.PHONY: go-mutants
go-mutants: ## Мутационное тестирование (долго, гоняй ночью)
	gremlins unleash --tags="" ./...

.PHONY: go
go: go-lint go-test go-vuln ## Полные ворота Go

# -------------------------------------------------------------- Rust ---------

.PHONY: rs-build
rs-build: ## Release-сборка с RUST_FEATURES
	cargo build --release --features $(RUST_FEATURES)

.PHONY: rs-check
rs-check: ## Быстрая проверка компиляции без бинарников
	cargo check --all-targets --features $(RUST_FEATURES)

.PHONY: rs-run
rs-run: ## Запустить сервер на REPOS (по умолчанию test-fixtures)
	cargo run --release --features $(RUST_FEATURES) -- --repos $(REPOS) --git --call-graph

.PHONY: rs-fmt-fix
rs-fmt-fix: ## Отформатировать код
	cargo fmt --all

.PHONY: rs-lint
rs-lint: ## clippy, предупреждения = ошибки (RUST_FEATURES, не --all-features)
	cargo clippy --all-targets --features $(RUST_FEATURES) -- -D warnings

.PHONY: rs-fmt
rs-fmt: ## Проверка форматирования
	cargo fmt --all --check

.PHONY: rs-test
rs-test: ## Тесты через nextest
	cargo nextest run --features $(RUST_FEATURES) --test-threads=$(JOBS)

.PHONY: rs-doc
rs-doc: ## Doc-тесты (nextest их не гоняет)
	cargo test --doc --features $(RUST_FEATURES)

.PHONY: rs-deny
rs-deny: ## Аудит зависимостей, лицензий и дублей
	cargo deny check

.PHONY: rs-unused
rs-unused: ## Неиспользуемые зависимости
	cargo machete

.PHONY: rs-cover
rs-cover: ## Покрытие
	cargo llvm-cov nextest --features $(RUST_FEATURES) --summary-only

.PHONY: rs-mutants
rs-mutants: ## Мутационное тестирование (долго, гоняй ночью)
	cargo mutants --jobs $(JOBS) --timeout 120

.PHONY: rs-miri
rs-miri: ## Проверка unsafe-кода под Miri
	cargo +nightly miri test

.PHONY: rs
rs: rs-fmt rs-lint rs-test rs-doc rs-deny ## Полные ворота Rust

# -------------------------------------------------------- Frontend ---------
# Нигде в CI не собирается — эта цель единственный способ проверить его локально.

.PHONY: frontend-build
frontend-build: ## Собрать UI визуализации (frontend/dist)
	cd frontend && npm ci && npm run build

.PHONY: frontend-test
frontend-test: ## Линт и тесты UI
	cd frontend && npm run lint && npm test

.PHONY: rs-build-frontend
rs-build-frontend: frontend-build ## Release со встроенным UI (фича frontend)
	cargo build --release --features $(RUST_FEATURES),frontend

# ------------------------------------------------------------ Eval -----------

.PHONY: eval-onec
eval-onec: rs-build ## Качество/задержка поиска на корпусе 1С (ONEC_REPO), см. скилл narsil-search-eval
	cd eval/ragdiag && .venv/bin/python -I onec/rerank_eval.py --repo $(ONEC_REPO)

# ------------------------------------------------------------ Python ---------
# test-fixtures/*.py — фикстуры для тестов парсера, не код проекта; py-цель
# ими не гоняем, оставлена на случай появления реальных Python-скриптов.

.PHONY: py
py: ## Линт и типы для скриптов (если появятся)
	ruff check .
	ruff format --check .
	@command -v pyright >/dev/null 2>&1 && pyright || echo "pyright не установлен, пропускаю"

# --------------------------------------------------------------- All ---------

.PHONY: version
version: ## Версия и коммит из git
	@echo "$(VERSION) ($(COMMIT))"

.PHONY: target-size
target-size: ## Размер target/: артефакты разрастались до 147 ГБ
	@du -sh target 2>/dev/null || echo "target/ нет"

.PHONY: clean
clean: ## Удалить target/ целиком (следующая сборка — с нуля)
	cargo clean

.PHONY: ci
ci: rs ## Всё, что должно быть зелёным перед коммитом (только Rust — см. выше)

.PHONY: nightly
nightly: rs-mutants ## Тяжёлые проверки: запускай через systemd-таймер
