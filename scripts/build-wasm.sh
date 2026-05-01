#!/bin/bash
# Build script for narsil-mcp WASM module
#
# Prerequisites:
# - Rust with wasm32-unknown-unknown target: rustup target add wasm32-unknown-unknown
# - wasm-pack: cargo install wasm-pack
# - (optional) wasm-opt for optimization: brew install binaryen or npm install -g wasm-opt
#
# Usage:
#   ./scripts/build-wasm.sh          # Build for web target
#   ./scripts/build-wasm.sh bundler  # Build for bundler (webpack, etc.)
#   ./scripts/build-wasm.sh nodejs   # Build for Node.js

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="$PROJECT_DIR/pkg"

# Default target
TARGET="${1:-web}"

echo "Building narsil-mcp WASM module..."
echo "  Target: $TARGET"
echo "  Output: $OUTPUT_DIR"
echo ""

# Validate target
case "$TARGET" in
  web|bundler|nodejs|deno)
    ;;
  *)
    echo "Error: Unknown target '$TARGET'"
    echo "Valid targets: web, bundler, nodejs, deno"
    exit 1
    ;;
esac

# Check for wasm-pack
if ! command -v wasm-pack &> /dev/null; then
    echo "Error: wasm-pack not found"
    echo "Install with: cargo install wasm-pack"
    exit 1
fi

# Check for wasm32 target
if ! rustup target list --installed | grep -q "wasm32-unknown-unknown"; then
    echo "Adding wasm32-unknown-unknown target..."
    rustup target add wasm32-unknown-unknown
fi

# Build with wasm-pack
echo "Building with wasm-pack..."
cd "$PROJECT_DIR"

wasm-pack build \
    --target "$TARGET" \
    --out-dir "$OUTPUT_DIR" \
    --out-name narsil_mcp_wasm \
    --features wasm \
    --no-default-features

# Optimize with wasm-opt if available
if command -v wasm-opt &> /dev/null; then
    WASM_FILE="$OUTPUT_DIR/narsil_mcp_wasm_bg.wasm"
    if [[ -f "$WASM_FILE" ]]; then
        echo ""
        echo "Optimizing WASM with wasm-opt..."
        ORIGINAL_SIZE=$(stat -f%z "$WASM_FILE" 2>/dev/null || stat -c%s "$WASM_FILE")

        wasm-opt -O3 -o "$WASM_FILE.opt" "$WASM_FILE"
        mv "$WASM_FILE.opt" "$WASM_FILE"

        OPTIMIZED_SIZE=$(stat -f%z "$WASM_FILE" 2>/dev/null || stat -c%s "$WASM_FILE")
        SAVED=$((ORIGINAL_SIZE - OPTIMIZED_SIZE))

        echo "  Original size: $(numfmt --to=iec "$ORIGINAL_SIZE" 2>/dev/null || echo "$ORIGINAL_SIZE bytes")"
        echo "  Optimized size: $(numfmt --to=iec "$OPTIMIZED_SIZE" 2>/dev/null || echo "$OPTIMIZED_SIZE bytes")"
        echo "  Saved: $(numfmt --to=iec "$SAVED" 2>/dev/null || echo "$SAVED bytes")"
    fi
else
    echo ""
    echo "Note: wasm-opt not found. Install for ~20% smaller WASM."
    echo "  brew install binaryen  # macOS"
    echo "  npm install -g wasm-opt  # npm"
fi

# Update package.json for npm publishing
echo ""
echo "Updating package.json..."

cat > "$OUTPUT_DIR/package.json" << 'EOF'
{
  "name": "@narsil-mcp/wasm",
  "version": "1.0.0",
  "description": "WebAssembly module for code intelligence - symbol extraction, search, and analysis",
  "main": "narsil_mcp_wasm.js",
  "module": "narsil_mcp_wasm.js",
  "types": "narsil_mcp_wasm.d.ts",
  "sideEffects": false,
  "files": [
    "narsil_mcp_wasm.js",
    "narsil_mcp_wasm.d.ts",
    "narsil_mcp_wasm_bg.wasm",
    "narsil_mcp_wasm_bg.wasm.d.ts",
    "index.js",
    "index.d.ts"
  ],
  "keywords": [
    "code",
    "intelligence",
    "wasm",
    "webassembly",
    "parser",
    "tree-sitter",
    "search",
    "symbols"
  ],
  "author": "Laurence",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "https://github.com/heiheshang/narsil-mcp.git"
  },
  "homepage": "https://github.com/heiheshang/narsil-mcp#readme",
  "bugs": {
    "url": "https://github.com/heiheshang/narsil-mcp/issues"
  }
}
EOF

# Create wrapper module with convenience exports
echo "Creating TypeScript wrapper..."

cat > "$OUTPUT_DIR/index.js" << 'EOF'
// Convenience wrapper for narsil-mcp WASM module
import init, { WasmCodeIntel, version } from './narsil_mcp_wasm.js';

export { WasmCodeIntel, version };

let initialized = false;
let initPromise = null;

/**
 * Initialize the WASM module. Must be called before using WasmCodeIntel.
 * Safe to call multiple times - subsequent calls are no-ops.
 */
export async function initialize() {
    if (initialized) return;
    if (initPromise) return initPromise;

    initPromise = init().then(() => {
        initialized = true;
    });

    return initPromise;
}

/**
 * Create a new code intelligence engine.
 * Automatically initializes WASM if not already done.
 */
export async function createEngine() {
    await initialize();
    return new WasmCodeIntel();
}

/**
 * Helper class providing a more TypeScript-friendly API
 */
export class CodeIntelClient {
    constructor() {
        this.engine = null;
        this.ready = false;
    }

    async init() {
        if (this.ready) return;
        await initialize();
        this.engine = new WasmCodeIntel();
        this.ready = true;
    }

    indexFile(path, content) {
        if (!this.engine) throw new Error('Client not initialized');
        return this.engine.index_file(path, content);
    }

    indexFiles(files) {
        if (!this.engine) throw new Error('Client not initialized');
        return this.engine.index_files(JSON.stringify(files));
    }

    findSymbols(pattern, kind) {
        if (!this.engine) return [];
        return JSON.parse(this.engine.find_symbols(pattern || null, kind || null));
    }

    search(query, maxResults = 10) {
        if (!this.engine) return [];
        return JSON.parse(this.engine.search(query, maxResults));
    }

    findSimilar(code, maxResults = 10) {
        if (!this.engine) return [];
        return JSON.parse(this.engine.find_similar(code, maxResults));
    }

    getFile(path) {
        return this.engine?.get_file(path) || null;
    }

    symbolAt(path, line) {
        if (!this.engine) return null;
        const json = this.engine.symbol_at(path, line);
        return json ? JSON.parse(json) : null;
    }

    symbolsInFile(path) {
        if (!this.engine) return [];
        return JSON.parse(this.engine.symbols_in_file(path));
    }

    listFiles() {
        if (!this.engine) return [];
        return JSON.parse(this.engine.list_files());
    }

    stats() {
        if (!this.engine) return { files: 0, symbols: 0, embeddings: 0 };
        return JSON.parse(this.engine.stats());
    }

    clear() {
        this.engine?.clear();
    }
}

export default { initialize, createEngine, CodeIntelClient, WasmCodeIntel, version };
EOF

# Create TypeScript declarations
cat > "$OUTPUT_DIR/index.d.ts" << 'EOF'
// TypeScript declarations for narsil-mcp WASM module

export interface Symbol {
    name: string;
    kind: string;
    file_path: string;
    start_line: number;
    end_line: number;
    signature?: string;
    qualified_name?: string;
    doc_comment?: string;
}

export interface SearchResult {
    file: string;
    start_line: number;
    end_line: number;
    content: string;
    score: number;
}

export interface SimilarCode {
    id: string;
    file: string;
    start_line: number;
    end_line: number;
    similarity: number;
}

export interface Stats {
    files: number;
    symbols: number;
    embeddings: number;
}

export interface FileInput {
    path: string;
    content: string;
}

/**
 * Initialize the WASM module. Must be called before using WasmCodeIntel.
 * Safe to call multiple times.
 */
export function initialize(): Promise<void>;

/**
 * Create a new code intelligence engine.
 * Automatically initializes WASM if needed.
 */
export function createEngine(): Promise<WasmCodeIntel>;

/**
 * Get the version of the WASM module.
 */
export function version(): string;

/**
 * Low-level WASM code intelligence engine.
 * For a friendlier API, use CodeIntelClient instead.
 */
export class WasmCodeIntel {
    constructor();

    /** Index a file from its content */
    index_file(path: string, content: string): boolean;

    /** Index multiple files at once (JSON array) */
    index_files(files_json: string): number;

    /** Find symbols matching pattern and/or kind */
    find_symbols(pattern: string | null, kind: string | null): string;

    /** Search code with BM25 ranking */
    search(query: string, max_results?: number): string;

    /** Find similar code using TF-IDF */
    find_similar(code: string, max_results?: number): string;

    /** Get file content */
    get_file(path: string): string | undefined;

    /** Get file content for a line range */
    get_file_lines(path: string, start_line: number, end_line: number): string | undefined;

    /** Get symbol at a line */
    symbol_at(path: string, line: number): string | undefined;

    /** Get all symbols in a file */
    symbols_in_file(path: string): string;

    /** List all indexed files */
    list_files(): string;

    /** Remove a file from the index */
    remove_file(path: string): boolean;

    /** Clear all indexed data */
    clear(): void;

    /** Get engine statistics */
    stats(): string;

    /** Get supported file extensions */
    supported_extensions(): string;
}

/**
 * High-level TypeScript-friendly client for code intelligence.
 */
export class CodeIntelClient {
    /** Whether the client is ready to use */
    ready: boolean;

    /** Initialize the client (must be called first) */
    init(): Promise<void>;

    /** Index a file */
    indexFile(path: string, content: string): boolean;

    /** Index multiple files */
    indexFiles(files: FileInput[]): number;

    /** Find symbols */
    findSymbols(pattern?: string, kind?: string): Symbol[];

    /** Search code */
    search(query: string, maxResults?: number): SearchResult[];

    /** Find similar code */
    findSimilar(code: string, maxResults?: number): SimilarCode[];

    /** Get file content */
    getFile(path: string): string | null;

    /** Get symbol at line */
    symbolAt(path: string, line: number): Symbol | null;

    /** Get all symbols in a file */
    symbolsInFile(path: string): Symbol[];

    /** List indexed files */
    listFiles(): string[];

    /** Get statistics */
    stats(): Stats;

    /** Clear all data */
    clear(): void;
}

declare const _default: {
    initialize: typeof initialize;
    createEngine: typeof createEngine;
    CodeIntelClient: typeof CodeIntelClient;
    WasmCodeIntel: typeof WasmCodeIntel;
    version: typeof version;
};

export default _default;
EOF

echo ""
echo "Build complete!"
echo ""
echo "Output files:"
ls -lh "$OUTPUT_DIR"/*.{js,wasm,d.ts,json} 2>/dev/null || ls -lh "$OUTPUT_DIR"

echo ""
echo "Usage example:"
echo ""
echo "  // In browser or bundler"
echo "  import { CodeIntelClient } from '@narsil-mcp/wasm';"
echo ""
echo "  const client = new CodeIntelClient();"
echo "  await client.init();"
echo ""
echo "  client.indexFile('src/main.rs', rustCode);"
echo "  const symbols = client.findSymbols('function');"
echo "  const results = client.search('error handling');"
echo ""

# Verify WASM size
WASM_FILE="$OUTPUT_DIR/narsil_mcp_wasm_bg.wasm"
if [[ -f "$WASM_FILE" ]]; then
    SIZE=$(stat -f%z "$WASM_FILE" 2>/dev/null || stat -c%s "$WASM_FILE")
    echo "Final WASM size: $(numfmt --to=iec "$SIZE" 2>/dev/null || echo "$SIZE bytes")"
fi
