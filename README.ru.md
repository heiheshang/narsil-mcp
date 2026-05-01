# narsil-mcp

> Быстрый локальный MCP-сервер для глубокой навигации по коду, поиска и анализа репозиториев

[English](README.md) | [Русский](README.ru.md)

## Что это

`narsil-mcp` - это MCP-сервер на Rust для AI-ассистентов и IDE. Он индексирует локальные репозитории, строит поисковые и структурные представления кода и отдает результаты через MCP, HTTP API и встроенные инструменты анализа.

Проект ориентирован на локальную работу: код не обязан покидать вашу машину, а индексирование и поиск работают быстро даже на больших репозиториях.

## Что умеет

- индексировать репозитории и автоматически находить символы, файлы и зависимости;
- выполнять keyword, semantic, hybrid и chunk-based поиск;
- строить call graph, CFG, data flow и искать dead code;
- запускать security и supply-chain анализ;
- отдавать инструменты через MCP, HTTP API и web UI;
- работать с несколькими репозиториями одновременно.

## Поддержка 1С

В `narsil-mcp` добавлена поддержка 1С-репозиториев и дампов конфигурации:

- поддерживается язык `BSL` (`.bsl`);
- распознаются типовые 1С-структуры вроде `Configuration.xml` и `ConfigDumpInfo.xml`;
- индексируются нормализованные документы, полученные из XML-метаданных 1С;
- в поиске участвуют как BSL-модули, так и артефакты конфигурационного дампа;
- результаты можно исследовать через `search_code`, `hybrid_search`, `search_chunks`, `get_chunks` и другие инструменты.

Это позволяет использовать `narsil-mcp` не только для обычных исходников, но и для навигации по 1С-конфигурациям.

## Поддерживаемые сценарии

- локальная работа в Cursor, Claude Desktop, VS Code, Zed и других MCP-клиентах;
- исследование больших mixed-language репозиториев;
- навигация по 1С/BSL и конфигурационным дампам;
- проверка индексации через HTTP API и web UI;
- установка бинарника из GitHub Releases через `curl | bash`.

## Быстрая установка

### Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/heiheshang/narsil-mcp/main/install.sh | bash
```

### Windows PowerShell

```powershell
irm https://raw.githubusercontent.com/heiheshang/narsil-mcp/main/install.ps1 | iex
```

### Сборка из исходников

```bash
git clone git@github.com:heiheshang/narsil-mcp.git
cd narsil-mcp
cargo build --release
```

Бинарник будет лежать в `target/release/narsil-mcp`.

## Запуск

Минимальный запуск:

```bash
./target/release/narsil-mcp --repos .
```

С HTTP API и web UI:

```bash
./target/release/narsil-mcp --repos . --http --persist --reindex
```

С дополнительными возможностями:

```bash
./target/release/narsil-mcp --repos . --git --call-graph --persist --watch
```

## Проверка индексации

После запуска с `--http` можно проверить:

```bash
curl -s http://localhost:3000/health
```

```bash
curl -s -X POST http://localhost:3000/tools/call \
  -H 'Content-Type: application/json' \
  -d '{"tool":"list_repos","args":{}}'
```

```bash
curl -s -X POST http://localhost:3000/tools/call \
  -H 'Content-Type: application/json' \
  -d '{"tool":"get_index_status","args":{}}'
```

## Релизы

GitHub Release публикует архивы, которые ожидает инсталлер:

- `narsil-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `narsil-mcp-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- `narsil-mcp-vX.Y.Z-x86_64-apple-darwin.tar.gz`
- `narsil-mcp-vX.Y.Z-windows-x86_64.zip`

Если готового бинарника для вашей платформы нет, инсталлер пытается перейти на установку из исходников через `cargo install --git`.

## Документация

- основной README: `README.md`
- установка: `docs/INSTALL.md`
- план поддержки 1С: `docs/1c-embedding-implementation-plan.md`
