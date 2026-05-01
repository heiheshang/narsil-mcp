/// Token usage benchmarks for MCP tools/list response
///
/// Analyzes the context window impact of different presets to answer:
/// "76 tools? Isn't that much too many? About how many tokens does Narsil
///  add to the context window with this many tools enabled?"
///
/// This benchmark shows the dramatic reduction in tokens when using presets:
/// - Full preset: ~76 tools, ~X tokens
/// - Balanced preset: ~45 tools, ~Y tokens (Z% reduction)
/// - Minimal preset: ~26 tools, ~W tokens (V% reduction)
///
/// Run with: cargo bench --bench token_usage
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use narsil_mcp::config::filter::{ClientInfo, ToolFilter};
use narsil_mcp::config::schema::ToolConfig;
use narsil_mcp::index::EngineOptions;
use narsil_mcp::tool_metadata::TOOL_METADATA;
use serde_json::json;
use std::time::Duration;

/// Estimate token count using OpenAI's rule of thumb (~4 chars per token)
/// This is accurate enough for comparison purposes
fn estimate_tokens(text: &str) -> usize {
    // Rule of thumb: 1 token ≈ 4 characters
    // More accurate: count words and punctuation, but this is close enough
    (text.len() as f64 / 4.0).ceil() as usize
}

/// Generate tools/list JSON response for a given preset
fn generate_tools_list_response(filter: &ToolFilter) -> serde_json::Value {
    let enabled_tools = filter.get_enabled_tools();

    let tools: Vec<serde_json::Value> = enabled_tools
        .iter()
        .filter_map(|name| TOOL_METADATA.get(name))
        .map(|meta| {
            json!({
                "name": meta.name,
                "description": meta.description,
                "inputSchema": meta.input_schema,
                "annotations": meta.mcp_annotations(),
            })
        })
        .collect();

    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": tools
        }
    })
}

/// Measure token usage for different presets
fn bench_token_usage_by_preset(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_usage");
    group.measurement_time(Duration::from_secs(3));

    // Define presets with their client names
    let presets = vec![
        ("minimal", "zed", "~26 tools"),
        ("balanced", "vscode", "~45 tools"),
        ("full", "claude-desktop", "~76 tools"),
        ("security-focused", "custom", "~30 tools"),
    ];

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📊 TOKEN USAGE ANALYSIS");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let mut results = Vec::new();

    for (preset_name, client_name, expected_tools) in &presets {
        // Create config with preset explicitly set
        let config = ToolConfig {
            preset: Some(preset_name.to_string()),
            ..Default::default()
        };

        // Enable all flags for fair comparison
        let options = EngineOptions {
            git_enabled: true,
            call_graph_enabled: true,
            lsp_config: narsil_mcp::lsp::LspConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };

        // Note: client_info is provided for completeness, but config.preset takes precedence
        let client_info = ClientInfo {
            name: client_name.to_string(),
            version: None,
        };

        let filter = ToolFilter::new(config, &options, Some(client_info));
        let response = generate_tools_list_response(&filter);
        let json_str = serde_json::to_string_pretty(&response).unwrap();

        let tool_count = filter.get_enabled_tools().len();
        let bytes = json_str.len();
        let estimated_tokens = estimate_tokens(&json_str);

        results.push((preset_name.to_string(), tool_count, bytes, estimated_tokens));

        println!(
            "  {} Preset: {}",
            match *preset_name {
                "minimal" => "⚡",
                "balanced" => "⚖️",
                "full" => "🔥",
                "security-focused" => "🔒",
                _ => "📦",
            },
            preset_name.to_uppercase()
        );
        println!("    Expected:        {}", expected_tools);
        println!("    Actual Tools:    {} tools", tool_count);
        println!(
            "    JSON Size:       {} bytes ({:.1} KB)",
            bytes,
            bytes as f64 / 1024.0
        );
        println!("    Estimated Tokens: ~{} tokens", estimated_tokens);
        println!();

        // Benchmark the serialization
        group.bench_with_input(
            BenchmarkId::from_parameter(preset_name),
            &filter,
            |b, filter| {
                b.iter(|| {
                    let response = generate_tools_list_response(black_box(filter));
                    let json_str = serde_json::to_string_pretty(&response).unwrap();
                    black_box(json_str);
                });
            },
        );
    }

    // Calculate and display reductions
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("💡 TOKEN SAVINGS");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    // Find full preset as baseline
    let full_tokens = results
        .iter()
        .find(|(name, _, _, _)| name == "full")
        .map(|(_, _, _, tokens)| *tokens)
        .unwrap_or(0);

    for (name, _tools, bytes, tokens) in &results {
        if name == "full" {
            continue; // Skip baseline
        }

        let token_reduction = full_tokens.saturating_sub(*tokens);
        let reduction_pct = if full_tokens > 0 {
            (token_reduction as f64 / full_tokens as f64) * 100.0
        } else {
            0.0
        };

        println!("  {} vs Full:", name.to_uppercase());
        println!(
            "    Token Reduction: {} tokens ({:.1}% smaller)",
            token_reduction, reduction_pct
        );
        println!(
            "    Bytes Saved:     {} bytes",
            results
                .iter()
                .find(|(n, _, _, _)| n == "full")
                .map(|(_, _, b, _)| *b)
                .unwrap_or(0)
                .saturating_sub(*bytes)
        );
        println!();
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("🎯 RECOMMENDATION");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    println!("  For editors with limited context windows (Zed, Cursor):");
    println!(
        "    → Use MINIMAL preset to save ~{}% tokens\n",
        results
            .iter()
            .find(|(n, _, _, _)| n == "minimal")
            .and_then(|_| results.iter().find(|(n, _, _, _)| n == "full"))
            .map(|(_, _, _, full_tokens)| {
                let minimal_tokens = results
                    .iter()
                    .find(|(n, _, _, _)| n == "minimal")
                    .map(|(_, _, _, t)| *t)
                    .unwrap_or(0);
                ((full_tokens - minimal_tokens) as f64 / *full_tokens as f64 * 100.0) as u32
            })
            .unwrap_or(0)
    );
    println!("  For general IDE usage (VS Code, IntelliJ):");
    println!(
        "    → Use BALANCED preset to save ~{}% tokens\n",
        results
            .iter()
            .find(|(n, _, _, _)| n == "balanced")
            .and_then(|_| results.iter().find(|(n, _, _, _)| n == "full"))
            .map(|(_, _, _, full_tokens)| {
                let balanced_tokens = results
                    .iter()
                    .find(|(n, _, _, _)| n == "balanced")
                    .map(|(_, _, _, t)| *t)
                    .unwrap_or(0);
                ((full_tokens - balanced_tokens) as f64 / *full_tokens as f64 * 100.0) as u32
            })
            .unwrap_or(0)
    );
    println!("  For AI assistants with large context (Claude Desktop):");
    println!("    → Use FULL preset for maximum capabilities\n");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    group.finish();
}

/// Benchmark just the filtering step (no JSON serialization)
fn bench_filtering_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("filtering_only");
    group.measurement_time(Duration::from_secs(3));

    let presets = vec![
        ("minimal", "zed"),
        ("balanced", "vscode"),
        ("full", "claude-desktop"),
    ];

    for (preset_name, client_name) in presets {
        group.bench_with_input(
            BenchmarkId::from_parameter(preset_name),
            &client_name,
            |b, &client| {
                let config = ToolConfig::default();
                let options = EngineOptions::default();
                let client_info = ClientInfo {
                    name: client.to_string(),
                    version: None,
                };

                b.iter(|| {
                    let filter = ToolFilter::new(
                        black_box(config.clone()),
                        black_box(&options),
                        black_box(Some(client_info.clone())),
                    );
                    let tools = filter.get_enabled_tools();
                    black_box(tools);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_token_usage_by_preset, bench_filtering_only,);
criterion_main!(benches);
