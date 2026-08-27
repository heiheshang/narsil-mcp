//! Control Flow Graph (CFG) analysis module.
//!
//! This module provides control flow graph construction and analysis for
//! detecting dead code, unreachable paths, and understanding program structure.
//!
//! # Features
//! - Basic block extraction from AST
//! - CFG construction with edges (branches, jumps, fall-through)
//! - Dominator tree computation
//! - Dead code detection
//! - Unreachable code detection
//! - Loop detection

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use tree_sitter::{Node, Tree};

/// Unique identifier for a basic block
pub type BlockId = usize;

/// A basic block in the control flow graph.
///
/// A basic block is a sequence of instructions with:
/// - One entry point (no jumps into the middle)
/// - One exit point (no jumps out of the middle)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasicBlock {
    /// Unique identifier for this block
    pub id: BlockId,
    /// Human-readable label for this block
    pub label: String,
    /// Starting line number in source
    pub start_line: usize,
    /// Ending line number in source
    pub end_line: usize,
    /// The type of terminator for this block
    pub terminator: Terminator,
    /// Statements/expressions in this block (simplified representation)
    pub statements: Vec<Statement>,
    /// Is this an entry block?
    pub is_entry: bool,
    /// Is this an exit block?
    pub is_exit: bool,
}

/// How a basic block terminates
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Terminator {
    /// Unconditional jump to another block
    Jump,
    /// Conditional branch (if/else, match)
    Branch {
        /// Condition expression (simplified)
        condition: String,
    },
    /// Return from function
    Return,
    /// Fall through to next block
    FallThrough,
    /// Loop back edge
    Loop,
    /// Break out of loop
    Break,
    /// Continue to next iteration
    Continue,
    /// Unreachable (after panic, etc.)
    Unreachable,
}

/// A simplified statement representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statement {
    /// Line number
    pub line: usize,
    /// Statement kind
    pub kind: StatementKind,
    /// Raw text (first 100 chars)
    pub text: String,
}

/// Types of statements we track
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatementKind {
    /// Variable declaration/assignment
    Assignment { variable: String },
    /// Expression statement
    Expression,
    /// Function call
    Call { function: String },
    /// Return statement
    Return,
    /// Control flow (if, match, loop, etc.)
    ControlFlow,
    /// Pattern binding (from match arms, for loops, if-let, while-let)
    PatternBinding { variables: Vec<String> },
    /// Other
    Other,
}

/// An edge in the control flow graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfgEdge {
    /// Source block
    pub from: BlockId,
    /// Target block
    pub to: BlockId,
    /// Edge type
    pub kind: EdgeKind,
}

/// Types of CFG edges
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Fall-through to next block
    FallThrough,
    /// Conditional branch (true path)
    TrueBranch,
    /// Conditional branch (false path)
    FalseBranch,
    /// Unconditional jump
    Jump,
    /// Loop back edge
    LoopBack,
    /// Loop exit
    LoopExit,
    /// Exception/error path
    Exception,
}

/// The control flow graph for a function
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFlowGraph {
    /// Function name
    pub function_name: String,
    /// File path
    pub file_path: String,
    /// Basic blocks indexed by ID
    pub blocks: HashMap<BlockId, BasicBlock>,
    /// Edges between blocks
    pub edges: Vec<CfgEdge>,
    /// Entry block ID
    pub entry_block: BlockId,
    /// Exit block IDs (may be multiple for early returns)
    pub exit_blocks: Vec<BlockId>,
    /// Dominator tree (block -> immediate dominator)
    pub dominators: HashMap<BlockId, BlockId>,
    /// Blocks that are unreachable from entry
    pub unreachable_blocks: Vec<BlockId>,
    /// Function parameters (treated as implicit definitions)
    pub parameters: Vec<String>,
}

impl ControlFlowGraph {
    /// Create an empty CFG
    pub fn new(function_name: &str, file_path: &str) -> Self {
        Self {
            function_name: function_name.to_string(),
            file_path: file_path.to_string(),
            blocks: HashMap::new(),
            edges: Vec::new(),
            entry_block: 0,
            exit_blocks: Vec::new(),
            dominators: HashMap::new(),
            unreachable_blocks: Vec::new(),
            parameters: Vec::new(),
        }
    }

    /// Create CFG with parameters
    pub fn new_with_params(function_name: &str, file_path: &str, parameters: Vec<String>) -> Self {
        let mut cfg = Self::new(function_name, file_path);
        cfg.parameters = parameters;
        cfg
    }

    /// Set function parameters
    pub fn set_parameters(&mut self, params: Vec<String>) {
        self.parameters = params;
    }

    /// Add a basic block to the CFG
    pub fn add_block(&mut self, block: BasicBlock) {
        let id = block.id;
        if block.is_entry {
            self.entry_block = id;
        }
        if block.is_exit {
            self.exit_blocks.push(id);
        }
        self.blocks.insert(id, block);
    }

    /// Add an edge between blocks
    pub fn add_edge(&mut self, from: BlockId, to: BlockId, kind: EdgeKind) {
        self.edges.push(CfgEdge { from, to, kind });
    }

    /// Get successors of a block
    pub fn successors(&self, block_id: BlockId) -> Vec<BlockId> {
        self.edges
            .iter()
            .filter(|e| e.from == block_id)
            .map(|e| e.to)
            .collect()
    }

    /// Get predecessors of a block
    pub fn predecessors(&self, block_id: BlockId) -> Vec<BlockId> {
        self.edges
            .iter()
            .filter(|e| e.to == block_id)
            .map(|e| e.from)
            .collect()
    }

    /// Compute dominator tree using Cooper's algorithm
    pub fn compute_dominators(&mut self) {
        if self.blocks.is_empty() {
            return;
        }

        // Initialize: entry dominates itself
        self.dominators.insert(self.entry_block, self.entry_block);

        // Get all blocks except entry in reverse postorder
        let block_ids: Vec<BlockId> = self.blocks.keys().copied().collect();

        // Iterate until fixed point
        let mut changed = true;
        while changed {
            changed = false;

            for &block_id in &block_ids {
                if block_id == self.entry_block {
                    continue;
                }

                let preds = self.predecessors(block_id);
                if preds.is_empty() {
                    continue;
                }

                // Find first predecessor with a dominator
                let mut new_idom = None;
                for &pred in &preds {
                    if self.dominators.contains_key(&pred) {
                        new_idom = Some(pred);
                        break;
                    }
                }

                if let Some(mut idom) = new_idom {
                    // Intersect with other predecessors
                    for &pred in &preds {
                        if self.dominators.contains_key(&pred) && pred != idom {
                            idom = self.intersect_dominators(pred, idom);
                        }
                    }

                    if self.dominators.get(&block_id) != Some(&idom) {
                        self.dominators.insert(block_id, idom);
                        changed = true;
                    }
                }
            }
        }
    }

    fn intersect_dominators(&self, b1: BlockId, b2: BlockId) -> BlockId {
        let mut finger1 = b1;
        let mut finger2 = b2;

        while finger1 != finger2 {
            while finger1 > finger2 {
                finger1 = *self.dominators.get(&finger1).unwrap_or(&finger1);
            }
            while finger2 > finger1 {
                finger2 = *self.dominators.get(&finger2).unwrap_or(&finger2);
            }
        }

        finger1
    }

    /// Find unreachable blocks (not reachable from entry)
    pub fn find_unreachable_blocks(&mut self) {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();

        queue.push_back(self.entry_block);
        reachable.insert(self.entry_block);

        while let Some(block_id) = queue.pop_front() {
            for succ in self.successors(block_id) {
                if !reachable.contains(&succ) {
                    reachable.insert(succ);
                    queue.push_back(succ);
                }
            }
        }

        self.unreachable_blocks = self
            .blocks
            .keys()
            .filter(|id| !reachable.contains(id))
            .copied()
            .collect();
    }

    /// Detect loops in the CFG (back edges)
    pub fn find_loops(&self) -> Vec<(BlockId, BlockId)> {
        let mut back_edges = Vec::new();

        for edge in &self.edges {
            // A back edge goes from a block to one of its dominators
            if self.dominates(edge.to, edge.from) {
                back_edges.push((edge.from, edge.to));
            }
        }

        back_edges
    }

    /// Check if block A dominates block B
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        if a == b {
            return true;
        }

        let mut current = b;
        while let Some(&dom) = self.dominators.get(&current) {
            if dom == a {
                return true;
            }
            if dom == current {
                break; // Entry node
            }
            current = dom;
        }

        false
    }

    /// Get all blocks in a loop (given the back edge header)
    pub fn get_loop_blocks(&self, header: BlockId, back_edge_source: BlockId) -> HashSet<BlockId> {
        let mut loop_blocks = HashSet::new();
        loop_blocks.insert(header);

        // Work backwards from back edge source to find all blocks in loop
        let mut worklist = vec![back_edge_source];

        while let Some(block) = worklist.pop() {
            if loop_blocks.contains(&block) {
                continue;
            }
            loop_blocks.insert(block);

            // Add predecessors that are dominated by header
            for pred in self.predecessors(block) {
                if !loop_blocks.contains(&pred) {
                    worklist.push(pred);
                }
            }
        }

        loop_blocks
    }

    /// Format CFG as markdown for AI consumption
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        md.push_str(&format!(
            "# Control Flow Graph: `{}`\n\n",
            self.function_name
        ));
        md.push_str(&format!("**File**: `{}`\n\n", self.file_path));
        md.push_str(&format!("**Blocks**: {}\n", self.blocks.len()));
        md.push_str(&format!("**Edges**: {}\n\n", self.edges.len()));

        // List blocks
        md.push_str("## Basic Blocks\n\n");
        let mut sorted_blocks: Vec<_> = self.blocks.values().collect();
        sorted_blocks.sort_by_key(|b| b.id);

        for block in sorted_blocks {
            let marker = if block.is_entry {
                " (ENTRY)"
            } else if block.is_exit {
                " (EXIT)"
            } else {
                ""
            };

            md.push_str(&format!(
                "### Block {}: {}{}\n\n",
                block.id, block.label, marker
            ));
            md.push_str(&format!(
                "Lines: {}-{}\n\n",
                block.start_line, block.end_line
            ));

            if !block.statements.is_empty() {
                md.push_str("```\n");
                for stmt in &block.statements {
                    // Statements are stored whole so data flow can read them;
                    // one line of a report is not the place to print all of a
                    // 400-character 1C call.
                    md.push_str(&format!(
                        "{:4}: {:?} - {}\n",
                        stmt.line,
                        stmt.kind,
                        crate::text::truncate_with_ellipsis(stmt.text.trim(), 160)
                    ));
                }
                md.push_str("```\n\n");
            }

            md.push_str(&format!("Terminator: `{:?}`\n\n", block.terminator));
        }

        // Show edges
        md.push_str("## Edges\n\n");
        for edge in &self.edges {
            md.push_str(&format!(
                "- Block {} -> Block {} ({:?})\n",
                edge.from, edge.to, edge.kind
            ));
        }
        md.push('\n');

        // Show unreachable blocks
        if !self.unreachable_blocks.is_empty() {
            md.push_str("## ⚠️ Unreachable Blocks\n\n");
            for &block_id in &self.unreachable_blocks {
                if let Some(block) = self.blocks.get(&block_id) {
                    md.push_str(&format!(
                        "- Block {}: lines {}-{}\n",
                        block_id, block.start_line, block.end_line
                    ));
                }
            }
            md.push('\n');
        }

        // Show loops
        let loops = self.find_loops();
        if !loops.is_empty() {
            md.push_str("## Loops Detected\n\n");
            for (back_source, header) in loops {
                md.push_str(&format!(
                    "- Loop at Block {} (back edge from Block {})\n",
                    header, back_source
                ));
            }
        }

        md
    }

    /// Export to DOT format for visualization
    pub fn to_dot(&self) -> String {
        let mut dot = String::from("digraph CFG {\n");
        dot.push_str("  rankdir=TB;\n");
        dot.push_str("  node [shape=box, fontname=\"monospace\"];\n\n");

        // Add nodes
        for block in self.blocks.values() {
            let shape = if block.is_entry || block.is_exit {
                "ellipse"
            } else {
                "box"
            };
            let color = if self.unreachable_blocks.contains(&block.id) {
                "red"
            } else if block.is_entry {
                "green"
            } else if block.is_exit {
                "blue"
            } else {
                "black"
            };

            dot.push_str(&format!(
                "  {} [label=\"{}\\n({}-{})\", shape={}, color={}];\n",
                block.id, block.label, block.start_line, block.end_line, shape, color
            ));
        }

        // Add edges
        for edge in &self.edges {
            let style = match edge.kind {
                EdgeKind::LoopBack => "dashed",
                EdgeKind::Exception => "dotted",
                _ => "solid",
            };
            let color = match edge.kind {
                EdgeKind::TrueBranch => "green",
                EdgeKind::FalseBranch => "red",
                EdgeKind::LoopBack => "blue",
                _ => "black",
            };

            dot.push_str(&format!(
                "  {} -> {} [style={}, color={}];\n",
                edge.from, edge.to, style, color
            ));
        }

        dot.push_str("}\n");
        dot
    }
}

/// CFG builder for constructing CFGs from tree-sitter ASTs
pub struct CfgBuilder {
    /// Current block ID counter
    next_block_id: BlockId,
    /// Current CFG being built
    cfg: ControlFlowGraph,
    /// Stack of loop headers for break/continue
    loop_stack: Vec<(BlockId, BlockId)>, // (header, exit)
}

impl CfgBuilder {
    /// Create a new CFG builder
    pub fn new(function_name: &str, file_path: &str) -> Self {
        Self {
            next_block_id: 0,
            cfg: ControlFlowGraph::new(function_name, file_path),
            loop_stack: Vec::new(),
        }
    }

    /// Create a new basic block
    pub fn create_block(&mut self, label: &str) -> BlockId {
        let id = self.next_block_id;
        self.next_block_id += 1;

        let block = BasicBlock {
            id,
            label: label.to_string(),
            start_line: 0,
            end_line: 0,
            terminator: Terminator::FallThrough,
            statements: Vec::new(),
            is_entry: false,
            is_exit: false,
        };

        self.cfg.add_block(block);
        id
    }

    /// Set a block as entry
    pub fn set_entry(&mut self, block_id: BlockId) {
        if let Some(block) = self.cfg.blocks.get_mut(&block_id) {
            block.is_entry = true;
            self.cfg.entry_block = block_id;
        }
    }

    /// Set a block as exit
    pub fn set_exit(&mut self, block_id: BlockId) {
        if let Some(block) = self.cfg.blocks.get_mut(&block_id) {
            block.is_exit = true;
            if !self.cfg.exit_blocks.contains(&block_id) {
                self.cfg.exit_blocks.push(block_id);
            }
        }
    }

    /// Add a statement to a block
    pub fn add_statement(&mut self, block_id: BlockId, stmt: Statement) {
        if let Some(block) = self.cfg.blocks.get_mut(&block_id) {
            // Update line range
            if block.statements.is_empty() {
                block.start_line = stmt.line;
                block.end_line = stmt.line;
            } else {
                block.start_line = block.start_line.min(stmt.line);
                block.end_line = block.end_line.max(stmt.line);
            }
            block.statements.push(stmt);
        }
    }

    /// Set terminator for a block
    pub fn set_terminator(&mut self, block_id: BlockId, terminator: Terminator) {
        if let Some(block) = self.cfg.blocks.get_mut(&block_id) {
            block.terminator = terminator;
        }
    }

    /// Add an edge
    pub fn add_edge(&mut self, from: BlockId, to: BlockId, kind: EdgeKind) {
        self.cfg.add_edge(from, to, kind);
    }

    /// Push a loop context
    pub fn push_loop(&mut self, header: BlockId, exit: BlockId) {
        self.loop_stack.push((header, exit));
    }

    /// Pop a loop context
    pub fn pop_loop(&mut self) {
        self.loop_stack.pop();
    }

    /// Get current loop header (for continue)
    pub fn current_loop_header(&self) -> Option<BlockId> {
        self.loop_stack.last().map(|(h, _)| *h)
    }

    /// Get current loop exit (for break)
    pub fn current_loop_exit(&self) -> Option<BlockId> {
        self.loop_stack.last().map(|(_, e)| *e)
    }

    /// Build and finalize the CFG
    pub fn build(mut self) -> ControlFlowGraph {
        self.cfg.compute_dominators();
        self.cfg.find_unreachable_blocks();
        self.cfg
    }

    /// Build CFG from a function AST node
    pub fn build_from_function(
        function_name: &str,
        file_path: &str,
        node: Node,
        source: &[u8],
    ) -> Result<ControlFlowGraph> {
        let mut builder = CfgBuilder::new(function_name, file_path);

        // Extract function parameters
        let params = extract_function_parameters(node, source);
        builder.cfg.set_parameters(params);

        // Create entry block
        let entry = builder.create_block("entry");
        builder.set_entry(entry);

        // Build CFG from the body, whichever shape the grammar gives it.
        let exit = builder
            .process_body_of(
                entry,
                node,
                source,
                &[
                    "block",
                    "function_body",
                    "statement_block",
                    "compound_statement",
                ],
            )?
            .ok_or_else(|| anyhow!("No function body found"))?;

        // Set exit
        builder.set_exit(exit);
        builder.set_terminator(exit, Terminator::Return);

        Ok(builder.build())
    }

    fn process_block_node(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let mut cursor = node.walk();
        let mut active_block = current;

        // Process children
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                active_block = self.process_statement(active_block, child, source)?;

                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        Ok(active_block)
    }

    /// Process the body of `node` into `block`, whichever shape the grammar
    /// gives it.
    ///
    /// Most grammars wrap a body in a node of its own (`block`,
    /// `statement_block`, `compound_statement`). BSL has no such node at all:
    /// the statements of a `Процедура`, an `Если` or a `Пока` are direct
    /// children of that construct. Every body site used to look only for the
    /// wrapper, so on BSL it found nothing and silently produced an empty
    /// branch — or, for a whole procedure, refused to build a CFG at all.
    ///
    /// Returns the block control reaches at the end of the body, or `None`
    /// when the construct genuinely has no body.
    fn process_body_of(
        &mut self,
        block: BlockId,
        node: Node,
        source: &[u8],
        wrappers: &[&str],
    ) -> Result<Option<BlockId>> {
        for wrapper in wrappers {
            if let Some(body) = find_child_by_kind(node, wrapper) {
                return Ok(Some(self.process_block_node(block, body, source)?));
            }
        }
        if !has_inline_statements(node) {
            return Ok(None);
        }
        Ok(Some(self.process_inline_body(block, node, source)?))
    }

    /// Process the statements hanging directly off `node`, skipping the
    /// keywords, name, parameters and condition around them.
    ///
    /// Clause wrappers (`else_clause`, `elseif_clause`, `except_clause`) are
    /// skipped too: each is a branch of its own and is wired up by the
    /// handler that owns it, not swallowed into the current block.
    fn process_inline_body(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let mut active = current;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_inline_statement(child.kind()) {
                active = self.process_statement(active, child, source)?;
            }
        }
        Ok(active)
    }

    fn process_statement(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let kind = node.kind();
        let line = node.start_position().row + 1;
        // The whole statement, not a prefix: the data-flow pass mines this
        // text for the variables the statement reads, so cutting it at 100
        // characters hid every read past that point. 1C statements are long
        // and its identifiers are whole words — `ОткрытьФорму(..., ЭтаФорма,
        // ..., ОписаниеОповещения, ...)` puts its reads well beyond the cut —
        // so the variables were reported as assigned and never used. Display
        // truncation belongs in the renderer, and lives there now.
        let text = node.utf8_text(source).unwrap_or("").to_string();

        match kind {
            // Control flow statements
            "if_statement" | "if_expression" => self.process_if(current, node, source),
            "while_statement" | "while_expression" => self.process_while(current, node, source),
            "for_statement" | "for_expression" => self.process_for(current, node, source),
            "loop_expression" => self.process_loop(current, node, source),
            "match_expression" => self.process_match(current, node, source),
            "return_statement" | "return_expression" => {
                // Return creates an exit
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::Return,
                        text,
                    },
                );
                self.set_terminator(current, Terminator::Return);
                self.set_exit(current);

                // Create a new block for code after return (unreachable)
                let next = self.create_block("after_return");
                Ok(next)
            }
            "break_statement" | "break_expression" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::ControlFlow,
                        text,
                    },
                );
                self.set_terminator(current, Terminator::Break);

                if let Some(exit) = self.current_loop_exit() {
                    self.add_edge(current, exit, EdgeKind::LoopExit);
                }

                let next = self.create_block("after_break");
                Ok(next)
            }
            "continue_statement" | "continue_expression" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::ControlFlow,
                        text,
                    },
                );
                self.set_terminator(current, Terminator::Continue);

                if let Some(header) = self.current_loop_header() {
                    self.add_edge(current, header, EdgeKind::LoopBack);
                }

                let next = self.create_block("after_continue");
                Ok(next)
            }
            // Regular statements (Rust)
            "let_declaration" | "let_statement" => {
                // Check if RHS contains control flow (e.g., let x = if cond { a } else { b })
                let has_cf_rhs = node.named_children(&mut node.walk()).any(|child| {
                    matches!(
                        child.kind(),
                        "if_expression" | "match_expression" | "loop_expression"
                    )
                });
                if has_cf_rhs {
                    // Add the assignment as a statement, then process the control flow child
                    let var_name = extract_variable_name(node, source).unwrap_or_default();
                    self.add_statement(
                        current,
                        Statement {
                            line,
                            kind: StatementKind::Assignment { variable: var_name },
                            text: text.clone(),
                        },
                    );
                    // Process the control flow RHS
                    let mut result = current;
                    for child in node.named_children(&mut node.walk()) {
                        match child.kind() {
                            "if_expression" => {
                                result = self.process_if(result, child, source)?;
                            }
                            "match_expression" => {
                                result = self.process_match(result, child, source)?;
                            }
                            _ => {}
                        }
                    }
                    return Ok(result);
                }
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_variable_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // Go variable declarations
            "short_var_declaration" => {
                // Go: x := 5 or x, y := foo()
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_go_short_var_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            "var_declaration" | "var_spec" => {
                // Go: var x = 5
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_go_var_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // Java/C# variable declarations
            "local_variable_declaration" => {
                // Java: int x = 5; or C#: var x = 5;
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_java_var_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // Kotlin variable declarations
            "property_declaration" => {
                // Kotlin: val x = 5 or var y = 10
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_kotlin_var_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // TypeScript/JavaScript variable declarations
            "lexical_declaration" | "variable_declaration" => {
                // TS: let x = 5; const y = 10; var z = 15;
                let stmt_kind = StatementKind::Assignment {
                    variable: extract_js_var_name(node, source).unwrap_or_default(),
                };
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // Expression statements may wrap control flow nodes (especially in Rust)
            "expression_statement" => {
                // Check if the first child is a control flow node and delegate
                if let Some(child) = node.named_child(0) {
                    match child.kind() {
                        "if_expression" | "if_statement" => {
                            return self.process_if(current, child, source);
                        }
                        "match_expression" => {
                            return self.process_match(current, child, source);
                        }
                        "while_expression" | "while_statement" => {
                            return self.process_while(current, child, source);
                        }
                        "for_expression" | "for_statement" => {
                            return self.process_for(current, child, source);
                        }
                        "loop_expression" => {
                            return self.process_loop(current, child, source);
                        }
                        "return_expression" | "return_statement" => {
                            self.add_statement(
                                current,
                                Statement {
                                    line,
                                    kind: StatementKind::Return,
                                    text,
                                },
                            );
                            self.set_terminator(current, Terminator::Return);
                            self.set_exit(current);
                            let next = self.create_block("after_return");
                            return Ok(next);
                        }
                        _ => {}
                    }
                }
                // Fall through: treat as generic expression statement
                let stmt_kind = if text.contains('=') {
                    StatementKind::Assignment {
                        variable: text.split('=').next().unwrap_or("").trim().to_string(),
                    }
                } else if text.contains('(') {
                    StatementKind::Call {
                        function: text.split('(').next().unwrap_or("").trim().to_string(),
                    }
                } else {
                    StatementKind::Expression
                };

                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // BSL: `Перем А, Б;` — names become defined with no value.
            // That is what `PatternBinding` already means downstream: the
            // data-flow pass records a definition with no right-hand side, so
            // reading such a variable before assigning it is visible.
            "var_statement" => {
                let mut cursor = node.walk();
                let variables: Vec<String> = node
                    .children_by_field_name("var_name", &mut cursor)
                    .filter_map(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string())
                    .collect();
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::PatternBinding { variables },
                        text,
                    },
                );
                Ok(current)
            }

            // BSL: `Х = ...;` is a statement, not an expression, and it names
            // its target in a field. Reaching definitions and dead-store
            // detection both key off `Assignment`, so without this arm the
            // whole data-flow layer saw a 1C module as a list of opaque
            // statements.
            "assignment_statement" => {
                let left = node.child_by_field_name("left");
                // Writing a property — `Запрос.Текст = ...`, `Элементы.X.Заголовок
                // = ...` — is the effect the code exists for, not a store into a
                // local that someone might forget to read. Recording it as an
                // assignment made the dead-store report accuse every form-setup
                // procedure in the configuration.
                let writes_property = left.is_some_and(|n| n.kind() == "property_access");
                let kind = if writes_property {
                    StatementKind::Expression
                } else {
                    let variable = left
                        .and_then(|n| n.utf8_text(source).ok())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| text.split('=').next().unwrap_or("").trim().to_string());
                    StatementKind::Assignment { variable }
                };
                self.add_statement(current, Statement { line, kind, text });
                Ok(current)
            }

            // BSL: a bare procedure call.
            "call_statement" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::Expression,
                        text,
                    },
                );
                Ok(current)
            }

            // BSL: `ВызватьИсключение` — the same shape as `throw`.
            "rise_error_statement" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::ControlFlow,
                        text,
                    },
                );
                self.set_terminator(current, Terminator::Jump);
                let next = self.create_block("after_raise");
                Ok(next)
            }

            // Assignment expressions (all languages)
            "assignment_expression" => {
                let stmt_kind = if text.contains('=') {
                    StatementKind::Assignment {
                        variable: text.split('=').next().unwrap_or("").trim().to_string(),
                    }
                } else {
                    StatementKind::Expression
                };

                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: stmt_kind,
                        text,
                    },
                );
                Ok(current)
            }
            // Block - process contents
            // Note: "statements" is Kotlin's container for statements inside function bodies
            "block" | "compound_statement" | "statements" => {
                self.process_block_node(current, node, source)
            }

            // ===================================================================================
            // Multi-Language Control Flow Support
            // ===================================================================================

            // Switch statements (Go, Java, C#)
            "switch_statement" | "expression_switch_statement" | "switch_expression" => {
                self.process_switch(current, node, source)
            }

            // Try-catch statements (Java, C#, Kotlin)
            "try_statement" | "try_expression" | "try_with_resources_statement" => {
                self.process_try_catch(current, node, source)
            }

            // When expression (Kotlin)
            "when_expression" => self.process_when(current, node, source),

            // Enhanced for loop (Java)
            "enhanced_for_statement" => self.process_enhanced_for(current, node, source),

            // Foreach statement (C#)
            "for_each_statement" | "foreach_statement" => {
                self.process_foreach(current, node, source)
            }

            // Do-while loop (Java, C#, Kotlin)
            "do_statement" | "do_while_statement" => self.process_do_while(current, node, source),

            // Defer statement (Go)
            "defer_statement" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::Other,
                        text: format!("defer: {}", text.chars().take(90).collect::<String>()),
                    },
                );
                Ok(current)
            }

            // Go statements (goroutine launch)
            "go_statement" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::Call {
                            function: "go".to_string(),
                        },
                        text,
                    },
                );
                Ok(current)
            }

            // Select statement (Go channels)
            "select_statement" => self.process_select(current, node, source),

            // Using statement (C#)
            "using_statement" => self.process_using(current, node, source),

            // Lock statement (C#)
            "lock_statement" => self.process_lock(current, node, source),

            // Throw statement (Java, C#, Kotlin)
            "throw_statement" | "throw_expression" => {
                self.add_statement(
                    current,
                    Statement {
                        line,
                        kind: StatementKind::ControlFlow,
                        text,
                    },
                );
                // Throw can potentially exit the function
                self.set_terminator(current, Terminator::Jump);
                let next = self.create_block("after_throw");
                Ok(next)
            }

            // Jump expression (Kotlin return, break, continue with labels)
            "jump_expression" => self.process_jump_expression(current, node, source),

            // Other
            _ => {
                // For other nodes, just add as expression if they have content
                if !text.is_empty() && text.len() > 1 {
                    self.add_statement(
                        current,
                        Statement {
                            line,
                            kind: StatementKind::Other,
                            text,
                        },
                    );
                }
                Ok(current)
            }
        }
    }

    fn process_if(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let line = node.start_position().row + 1;
        let condition = extract_condition(node, source).unwrap_or_default();

        // Add condition check to current block
        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: format!("if {}", condition),
            },
        );
        self.set_terminator(
            current,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        // Create blocks
        let then_block = self.create_block("then");
        let merge_block = self.create_block("endif");

        // True branch
        self.add_edge(current, then_block, EdgeKind::TrueBranch);

        // Check for if-let pattern and extract bindings
        // In Rust tree-sitter, if-let has a "let_condition" or the pattern is in "let_chain"
        if let Some(pattern) = find_if_let_pattern(node) {
            let bindings = extract_pattern_bindings(pattern, source);
            if !bindings.is_empty() {
                let pattern_text = pattern.utf8_text(source).unwrap_or("pattern").to_string();
                self.add_statement(
                    then_block,
                    Statement {
                        line: pattern.start_position().row + 1,
                        kind: StatementKind::PatternBinding {
                            variables: bindings,
                        },
                        text: format!(
                            "if let {}",
                            pattern_text.chars().take(85).collect::<String>()
                        ),
                    },
                );
            }
        }

        // Process then branch
        match self.process_body_of(then_block, node, source, &["block", "consequence"])? {
            Some(then_exit) => self.add_edge(then_exit, merge_block, EdgeKind::FallThrough),
            None => self.add_edge(then_block, merge_block, EdgeKind::FallThrough),
        }

        // Walk the `else if` chain, if any, hanging the next test off the
        // previous one's false edge. BSL spells these `elseif_clause` and they
        // are extremely common in 1C; without this their statements were
        // dropped from the graph entirely rather than appearing on a branch.
        let mut false_from = current;
        let mut cursor = node.walk();
        let elseif_clauses: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|child| matches!(child.kind(), "elseif_clause" | "else_if_clause"))
            .collect();

        for clause in elseif_clauses {
            let test_block = self.create_block("elseif");
            self.add_edge(false_from, test_block, EdgeKind::FalseBranch);

            let condition = extract_condition(clause, source).unwrap_or_default();
            self.add_statement(
                test_block,
                Statement {
                    line: clause.start_position().row + 1,
                    kind: StatementKind::ControlFlow,
                    text: format!("else if {}", condition),
                },
            );
            self.set_terminator(test_block, Terminator::Branch { condition });

            let body_block = self.create_block("elseif_body");
            self.add_edge(test_block, body_block, EdgeKind::TrueBranch);
            match self.process_body_of(body_block, clause, source, &["block", "consequence"])? {
                Some(body_exit) => self.add_edge(body_exit, merge_block, EdgeKind::FallThrough),
                None => self.add_edge(body_block, merge_block, EdgeKind::FallThrough),
            }

            false_from = test_block;
        }

        // Check for else
        if let Some(else_clause) = find_child_by_kind(node, "else_clause")
            .or_else(|| find_child_by_kind(node, "alternative"))
        {
            let else_block = self.create_block("else");
            self.add_edge(false_from, else_block, EdgeKind::FalseBranch);

            let else_exit = match self.process_body_of(
                else_block,
                else_clause,
                source,
                &["block", "consequence"],
            )? {
                Some(exit) => exit,
                // Grammars that put the else body straight in the clause:
                // Rust's `else_clause` holds a `block`, or a nested
                // `if_expression` for `else if`.
                None => self.process_block_node(else_block, else_clause, source)?,
            };
            self.add_edge(else_exit, merge_block, EdgeKind::FallThrough);
        } else {
            // No else - the last false branch goes directly to merge
            self.add_edge(false_from, merge_block, EdgeKind::FalseBranch);
        }

        Ok(merge_block)
    }

    fn process_while(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let condition = extract_condition(node, source).unwrap_or_default();

        // Header block (condition check)
        let header = self.create_block("while_header");
        self.add_edge(current, header, EdgeKind::FallThrough);

        self.add_statement(
            header,
            Statement {
                line: node.start_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: format!("while {}", condition),
            },
        );
        self.set_terminator(
            header,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        // Body and exit blocks
        let body_block = self.create_block("while_body");
        let exit_block = self.create_block("while_exit");

        // Push loop context
        self.push_loop(header, exit_block);

        // Edges
        self.add_edge(header, body_block, EdgeKind::TrueBranch);
        self.add_edge(header, exit_block, EdgeKind::FalseBranch);

        // Check for while-let pattern and extract bindings
        if let Some(pattern) = find_while_let_pattern(node) {
            let bindings = extract_pattern_bindings(pattern, source);
            if !bindings.is_empty() {
                let pattern_text = pattern.utf8_text(source).unwrap_or("pattern").to_string();
                self.add_statement(
                    body_block,
                    Statement {
                        line: pattern.start_position().row + 1,
                        kind: StatementKind::PatternBinding {
                            variables: bindings,
                        },
                        text: format!(
                            "while let {}",
                            pattern_text.chars().take(82).collect::<String>()
                        ),
                    },
                );
            }
        }

        // Process body
        match self.process_body_of(body_block, node, source, &["block"])? {
            Some(body_exit) => self.add_edge(body_exit, header, EdgeKind::LoopBack),
            None => self.add_edge(body_block, header, EdgeKind::LoopBack),
        }

        self.pop_loop();

        Ok(exit_block)
    }

    fn process_for(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        // Similar to while but with initialization
        let header = self.create_block("for_header");
        self.add_edge(current, header, EdgeKind::FallThrough);

        self.add_statement(
            header,
            Statement {
                line: node.start_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: "for loop".to_string(),
            },
        );
        self.set_terminator(header, Terminator::Loop);

        let body_block = self.create_block("for_body");
        let exit_block = self.create_block("for_exit");

        self.push_loop(header, exit_block);

        self.add_edge(header, body_block, EdgeKind::TrueBranch);
        self.add_edge(header, exit_block, EdgeKind::FalseBranch);

        // Extract loop variable pattern (for pattern in iterable { ... })
        if let Some(pattern) = find_for_loop_pattern(node) {
            let bindings = extract_pattern_bindings(pattern, source);
            if !bindings.is_empty() {
                let pattern_text = pattern.utf8_text(source).unwrap_or("pattern").to_string();
                self.add_statement(
                    body_block,
                    Statement {
                        line: pattern.start_position().row + 1,
                        kind: StatementKind::PatternBinding {
                            variables: bindings,
                        },
                        text: format!("for {}", pattern_text.chars().take(90).collect::<String>()),
                    },
                );
            }
        }

        match self.process_body_of(body_block, node, source, &["block"])? {
            Some(body_exit) => self.add_edge(body_exit, header, EdgeKind::LoopBack),
            None => self.add_edge(body_block, header, EdgeKind::LoopBack),
        }

        self.pop_loop();

        Ok(exit_block)
    }

    fn process_loop(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        // Infinite loop (loop {})
        let header = self.create_block("loop_header");
        self.add_edge(current, header, EdgeKind::FallThrough);

        self.add_statement(
            header,
            Statement {
                line: node.start_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: "loop".to_string(),
            },
        );
        self.set_terminator(header, Terminator::Loop);

        let body_block = self.create_block("loop_body");
        let exit_block = self.create_block("loop_exit");

        self.push_loop(header, exit_block);

        self.add_edge(header, body_block, EdgeKind::FallThrough);

        match self.process_body_of(body_block, node, source, &["block"])? {
            Some(body_exit) => self.add_edge(body_exit, header, EdgeKind::LoopBack),
            None => self.add_edge(body_block, header, EdgeKind::LoopBack),
        }

        self.pop_loop();

        Ok(exit_block)
    }

    fn process_match(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let condition = extract_condition(node, source).unwrap_or_default();

        self.add_statement(
            current,
            Statement {
                line: node.start_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: format!("match {}", condition),
            },
        );
        self.set_terminator(
            current,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        let merge = self.create_block("match_end");

        // Process each arm
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            let mut arm_count = 0;
            loop {
                if cursor.node().kind() == "match_arm" {
                    arm_count += 1;
                    let arm_block = self.create_block(&format!("match_arm_{}", arm_count));
                    self.add_edge(current, arm_block, EdgeKind::Jump);

                    // Extract pattern bindings from the match arm pattern
                    let arm_node = cursor.node();
                    if let Some(pattern) = find_match_arm_pattern(arm_node) {
                        let bindings = extract_pattern_bindings(pattern, source);
                        if !bindings.is_empty() {
                            let pattern_text =
                                pattern.utf8_text(source).unwrap_or("pattern").to_string();
                            self.add_statement(
                                arm_block,
                                Statement {
                                    line: pattern.start_position().row + 1,
                                    kind: StatementKind::PatternBinding {
                                        variables: bindings,
                                    },
                                    text: pattern_text.chars().take(100).collect(),
                                },
                            );
                        }
                    }

                    if let Some(body) = find_child_by_kind(cursor.node(), "block") {
                        let arm_exit = self.process_block_node(arm_block, body, source)?;
                        self.add_edge(arm_exit, merge, EdgeKind::FallThrough);
                    } else {
                        // Handle expression arms (no block, direct expression)
                        self.add_edge(arm_block, merge, EdgeKind::FallThrough);
                    }
                }

                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        Ok(merge)
    }

    // ===================================================================================
    // Multi-Language Control Flow Processing Methods
    // ===================================================================================

    /// Process switch statements (Go, Java, C#)
    fn process_switch(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let condition = extract_condition(node, source).unwrap_or_default();
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: format!("switch {}", condition),
            },
        );
        self.set_terminator(
            current,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        let merge = self.create_block("switch_end");
        let mut has_default = false;

        // Helper function to process case nodes at any depth
        fn process_switch_cases(
            builder: &mut CfgBuilder,
            node: Node,
            source: &[u8],
            current: BlockId,
            merge: BlockId,
            case_count: &mut usize,
            has_default: &mut bool,
        ) -> Result<()> {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    let child_kind = child.kind();

                    // Go: expression_case, default_case
                    // Java: switch_block_statement_group (inside switch_block)
                    // C#: switch_section
                    if child_kind == "expression_case"
                        || child_kind == "default_case"
                        || child_kind == "switch_block_statement_group"
                        || child_kind == "switch_section"
                    {
                        *case_count += 1;
                        let is_default = child_kind == "default_case"
                            || child.utf8_text(source).unwrap_or("").contains("default:");

                        if is_default {
                            *has_default = true;
                        }

                        let case_label = if is_default {
                            "default".to_string()
                        } else {
                            format!("case_{}", *case_count)
                        };

                        let case_block = builder.create_block(&case_label);
                        builder.add_edge(current, case_block, EdgeKind::Jump);

                        // Process case body
                        let case_exit = builder.process_block_node(case_block, child, source)?;
                        builder.add_edge(case_exit, merge, EdgeKind::FallThrough);
                    }
                    // Java: switch_block, C#: switch_body - contain the cases
                    else if child_kind == "switch_block" || child_kind == "switch_body" {
                        process_switch_cases(
                            builder,
                            child,
                            source,
                            current,
                            merge,
                            case_count,
                            has_default,
                        )?;
                    }

                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            Ok(())
        }

        let mut case_count = 0;
        process_switch_cases(
            self,
            node,
            source,
            current,
            merge,
            &mut case_count,
            &mut has_default,
        )?;

        // If no default case, add edge from current to merge
        if !has_default {
            self.add_edge(current, merge, EdgeKind::FalseBranch);
        }

        Ok(merge)
    }

    /// Process try-catch statements (Java, C#, Kotlin)
    fn process_try_catch(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: "try".to_string(),
            },
        );

        let try_block = self.create_block("try_body");
        let merge = self.create_block("try_end");

        self.add_edge(current, try_block, EdgeKind::FallThrough);

        // BSL has no clause wrappers: the `Попытка` and `Исключение`
        // statements are all direct children of `try_statement`, separated
        // only by the keyword token. Searching for a `catch_clause` that
        // cannot exist dropped both halves of the construct from the graph.
        if let Some(except_kw) = find_child_by_kind(node, "EXCEPT_KEYWORD") {
            let split = except_kw.start_byte();
            let mut cursor = node.walk();
            let (guarded, handler): (Vec<Node>, Vec<Node>) = node
                .named_children(&mut cursor)
                .filter(|child| is_inline_statement(child.kind()))
                .partition(|child| child.start_byte() < split);

            let mut try_exit = try_block;
            for stmt in guarded {
                try_exit = self.process_statement(try_exit, stmt, source)?;
            }
            self.add_edge(try_exit, merge, EdgeKind::FallThrough);

            let handler_block = self.create_block("except");
            // The exception edge leaves the guarded region, not its exit.
            self.add_edge(try_block, handler_block, EdgeKind::Jump);
            let mut handler_exit = handler_block;
            for stmt in handler {
                handler_exit = self.process_statement(handler_exit, stmt, source)?;
            }
            self.add_edge(handler_exit, merge, EdgeKind::FallThrough);

            return Ok(merge);
        }

        // Process try body and catch/finally clauses
        let mut cursor = node.walk();
        let mut try_exit = try_block;
        let mut catch_blocks = Vec::new();

        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let child_kind = child.kind();

                // Try block body
                if child_kind == "block" && try_exit == try_block {
                    try_exit = self.process_block_node(try_block, child, source)?;
                }
                // Catch clause (Java, C#)
                else if child_kind == "catch_clause" {
                    let catch_block = self.create_block("catch");
                    catch_blocks.push(catch_block);
                    self.add_edge(try_block, catch_block, EdgeKind::Jump); // Exception edge

                    if let Some(body) = find_child_by_kind(child, "block") {
                        let catch_exit = self.process_block_node(catch_block, body, source)?;
                        self.add_edge(catch_exit, merge, EdgeKind::FallThrough);
                    } else {
                        self.add_edge(catch_block, merge, EdgeKind::FallThrough);
                    }
                }
                // Kotlin catch clause
                else if child_kind == "catch_block" {
                    let catch_block = self.create_block("catch");
                    catch_blocks.push(catch_block);
                    self.add_edge(try_block, catch_block, EdgeKind::Jump);

                    let catch_exit = self.process_block_node(catch_block, child, source)?;
                    self.add_edge(catch_exit, merge, EdgeKind::FallThrough);
                }
                // Finally clause
                else if child_kind == "finally_clause" || child_kind == "finally_block" {
                    let finally_block = self.create_block("finally");

                    // Connect try exit to finally
                    // Note: catch exits go directly to merge (simplified model)
                    // A full implementation would re-route catch exits through finally
                    self.add_edge(try_exit, finally_block, EdgeKind::FallThrough);
                    drop(catch_blocks); // Catch exits already connected to merge above

                    if let Some(body) = find_child_by_kind(child, "block") {
                        let finally_exit = self.process_block_node(finally_block, body, source)?;
                        self.add_edge(finally_exit, merge, EdgeKind::FallThrough);
                    } else {
                        let finally_exit = self.process_block_node(finally_block, child, source)?;
                        self.add_edge(finally_exit, merge, EdgeKind::FallThrough);
                    }

                    return Ok(merge);
                }

                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        // No finally - connect try exit to merge
        self.add_edge(try_exit, merge, EdgeKind::FallThrough);

        Ok(merge)
    }

    /// Process when expression (Kotlin)
    fn process_when(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let condition = extract_condition(node, source).unwrap_or_default();
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: format!("when {}", condition),
            },
        );
        self.set_terminator(
            current,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        let merge = self.create_block("when_end");
        let mut has_else = false;

        // Process each when entry
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            let mut entry_count = 0;
            loop {
                let child = cursor.node();

                if child.kind() == "when_entry" {
                    entry_count += 1;

                    // Check if this is an else entry
                    let is_else = child
                        .utf8_text(source)
                        .unwrap_or("")
                        .trim_start()
                        .starts_with("else");
                    if is_else {
                        has_else = true;
                    }

                    let entry_label = if is_else {
                        "when_else".to_string()
                    } else {
                        format!("when_entry_{}", entry_count)
                    };

                    let entry_block = self.create_block(&entry_label);
                    self.add_edge(current, entry_block, EdgeKind::Jump);

                    // Process entry body
                    if let Some(body) = find_child_by_kind(child, "control_structure_body") {
                        let entry_exit = self.process_block_node(entry_block, body, source)?;
                        self.add_edge(entry_exit, merge, EdgeKind::FallThrough);
                    } else {
                        let entry_exit = self.process_block_node(entry_block, child, source)?;
                        self.add_edge(entry_exit, merge, EdgeKind::FallThrough);
                    }
                }

                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        // If no else, add edge from current to merge
        if !has_else {
            self.add_edge(current, merge, EdgeKind::FalseBranch);
        }

        Ok(merge)
    }

    /// Process enhanced for loop (Java: for (Type x : iterable))
    fn process_enhanced_for(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let header = self.create_block("enhanced_for_header");
        self.add_edge(current, header, EdgeKind::FallThrough);

        // The header's own source, not a synthetic label: the data-flow pass
        // mines a statement's text for the variables it reads, so the literal
        // string "enhanced for" was read back as a variable named `enhanced`
        // and reported as never initialised.
        self.add_statement(
            header,
            Statement {
                line: node.start_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: loop_header_text(node, source),
            },
        );
        self.set_terminator(header, Terminator::Loop);

        let body_block = self.create_block("enhanced_for_body");
        let exit_block = self.create_block("enhanced_for_exit");

        self.push_loop(header, exit_block);

        self.add_edge(header, body_block, EdgeKind::TrueBranch);
        self.add_edge(header, exit_block, EdgeKind::FalseBranch);

        // The iteration variable is bound on entry to the body. Without this
        // it read as a variable used before any assignment — on every
        // iteration of every `Для Каждого`.
        if let Some(var) = find_child_by_kind(node, "identifier")
            .and_then(|n| n.utf8_text(source).ok())
            .map(|s| s.to_string())
        {
            self.add_statement(
                body_block,
                Statement {
                    line: node.start_position().row + 1,
                    kind: StatementKind::PatternBinding {
                        variables: vec![var],
                    },
                    text: loop_header_text(node, source),
                },
            );
        }

        // Process body
        match self.process_body_of(body_block, node, source, &["block", "statement"])? {
            Some(body_exit) => self.add_edge(body_exit, header, EdgeKind::LoopBack),
            None => self.add_edge(body_block, header, EdgeKind::LoopBack),
        }

        self.pop_loop();

        Ok(exit_block)
    }

    /// Process foreach statement (C#: foreach (var x in collection))
    fn process_foreach(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        // Similar to enhanced for
        self.process_enhanced_for(current, node, source)
    }

    /// Process do-while loop (Java, C#, Kotlin)
    fn process_do_while(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let body_block = self.create_block("do_body");
        let condition_block = self.create_block("do_condition");
        let exit_block = self.create_block("do_exit");

        self.add_edge(current, body_block, EdgeKind::FallThrough);

        self.push_loop(condition_block, exit_block);

        // Process body first (do-while executes body at least once)
        match self.process_body_of(body_block, node, source, &["block"])? {
            Some(body_exit) => self.add_edge(body_exit, condition_block, EdgeKind::FallThrough),
            None => self.add_edge(body_block, condition_block, EdgeKind::FallThrough),
        }

        // Condition
        let condition = extract_do_while_condition(node, source).unwrap_or_default();
        self.add_statement(
            condition_block,
            Statement {
                line: node.end_position().row + 1,
                kind: StatementKind::ControlFlow,
                text: format!("while {}", condition),
            },
        );
        self.set_terminator(
            condition_block,
            Terminator::Branch {
                condition: condition.clone(),
            },
        );

        self.add_edge(condition_block, body_block, EdgeKind::TrueBranch);
        self.add_edge(condition_block, exit_block, EdgeKind::FalseBranch);

        self.pop_loop();

        Ok(exit_block)
    }

    /// Process select statement (Go channel operations)
    fn process_select(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: "select".to_string(),
            },
        );
        self.set_terminator(
            current,
            Terminator::Branch {
                condition: "channel".to_string(),
            },
        );

        let merge = self.create_block("select_end");

        // Process each communication clause
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            let mut case_count = 0;
            loop {
                let child = cursor.node();
                if child.kind() == "communication_case" || child.kind() == "default_case" {
                    case_count += 1;
                    let case_block = self.create_block(&format!("select_case_{}", case_count));
                    self.add_edge(current, case_block, EdgeKind::Jump);

                    let case_exit = self.process_block_node(case_block, child, source)?;
                    self.add_edge(case_exit, merge, EdgeKind::FallThrough);
                }

                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        Ok(merge)
    }

    /// Process using statement (C# resource management)
    fn process_using(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: "using".to_string(),
            },
        );

        // Process the using body
        if let Some(body) = find_child_by_kind(node, "block") {
            self.process_block_node(current, body, source)
        } else {
            Ok(current)
        }
    }

    /// Process lock statement (C# synchronization)
    fn process_lock(&mut self, current: BlockId, node: Node, source: &[u8]) -> Result<BlockId> {
        let line = node.start_position().row + 1;

        self.add_statement(
            current,
            Statement {
                line,
                kind: StatementKind::ControlFlow,
                text: "lock".to_string(),
            },
        );

        // Process the lock body
        if let Some(body) = find_child_by_kind(node, "block") {
            self.process_block_node(current, body, source)
        } else {
            Ok(current)
        }
    }

    /// Process jump expression (Kotlin return/break/continue with optional labels)
    fn process_jump_expression(
        &mut self,
        current: BlockId,
        node: Node,
        source: &[u8],
    ) -> Result<BlockId> {
        let text = node.utf8_text(source).unwrap_or("").to_string();
        let line = node.start_position().row + 1;

        // Determine the type of jump
        if text.starts_with("return") {
            // Check if this return contains a when expression (Kotlin: return when (x) { ... })
            if let Some(when_node) = find_child_by_kind(node, "when_expression") {
                // Process the when expression first, then mark the merge block as a return
                let when_exit = self.process_when(current, when_node, source)?;
                self.add_statement(
                    when_exit,
                    Statement {
                        line,
                        kind: StatementKind::Return,
                        text: "return (from when)".to_string(),
                    },
                );
                self.set_terminator(when_exit, Terminator::Return);
                self.set_exit(when_exit);
                let next = self.create_block("after_return");
                return Ok(next);
            }

            self.add_statement(
                current,
                Statement {
                    line,
                    kind: StatementKind::Return,
                    text,
                },
            );
            self.set_terminator(current, Terminator::Return);
            self.set_exit(current);
            let next = self.create_block("after_return");
            Ok(next)
        } else if text.starts_with("break") {
            self.add_statement(
                current,
                Statement {
                    line,
                    kind: StatementKind::ControlFlow,
                    text,
                },
            );
            self.set_terminator(current, Terminator::Break);
            if let Some(exit) = self.current_loop_exit() {
                self.add_edge(current, exit, EdgeKind::LoopExit);
            }
            let next = self.create_block("after_break");
            Ok(next)
        } else if text.starts_with("continue") {
            self.add_statement(
                current,
                Statement {
                    line,
                    kind: StatementKind::ControlFlow,
                    text,
                },
            );
            self.set_terminator(current, Terminator::Continue);
            if let Some(header) = self.current_loop_header() {
                self.add_edge(current, header, EdgeKind::LoopBack);
            }
            let next = self.create_block("after_continue");
            Ok(next)
        } else {
            // Unknown jump type, treat as expression
            self.add_statement(
                current,
                Statement {
                    line,
                    kind: StatementKind::Other,
                    text,
                },
            );
            Ok(current)
        }
    }
}

/// Extract condition from do-while statement
fn extract_do_while_condition(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            // Look for parenthesized expression or condition after 'while'
            if child.kind() == "parenthesized_expression"
                || child.kind() == "condition"
                || child.kind() == "while_condition"
            {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Find the pattern node within a match arm
fn find_match_arm_pattern(arm_node: Node) -> Option<Node> {
    let mut cursor = arm_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();
            // Match arm patterns have specific node types
            if kind.contains("pattern")
                || kind == "identifier"
                || kind == "tuple_struct_pattern"
                || kind == "struct_pattern"
                || kind == "tuple_pattern"
                || kind == "slice_pattern"
                || kind == "or_pattern"
            {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Find the pattern node within a for expression (for pattern in iterable)
fn find_for_loop_pattern(for_node: Node) -> Option<Node> {
    let mut cursor = for_node.walk();
    if cursor.goto_first_child() {
        // Skip the 'for' keyword
        loop {
            let child = cursor.node();
            let kind = child.kind();
            // The pattern comes after 'for' keyword and before 'in'
            if kind.contains("pattern")
                || kind == "identifier"
                || kind == "tuple_pattern"
                || kind == "struct_pattern"
                || kind == "slice_pattern"
                || kind == "mut_pattern"
            {
                return Some(child);
            }
            // Don't go past the 'in' keyword
            if kind == "in" {
                break;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Find the pattern node within an if-let expression
/// Handles: if let pattern = expr { ... }
fn find_if_let_pattern(if_node: Node) -> Option<Node> {
    let mut cursor = if_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();

            // Look for let_condition which contains the pattern
            if kind == "let_condition" || kind == "let_chain" {
                // Recurse into let_condition to find the pattern
                let mut inner_cursor = child.walk();
                if inner_cursor.goto_first_child() {
                    loop {
                        let inner_child = inner_cursor.node();
                        let inner_kind = inner_child.kind();
                        if inner_kind.contains("pattern")
                            || inner_kind == "tuple_struct_pattern"
                            || inner_kind == "struct_pattern"
                            || inner_kind == "tuple_pattern"
                            || inner_kind == "slice_pattern"
                        {
                            return Some(inner_child);
                        }
                        if !inner_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            // Some tree-sitter grammars put the pattern directly under if_expression
            if kind.contains("pattern")
                || kind == "tuple_struct_pattern"
                || kind == "struct_pattern"
                || kind == "tuple_pattern"
            {
                return Some(child);
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Find the pattern node within a while-let expression
/// Handles: while let pattern = expr { ... }
fn find_while_let_pattern(while_node: Node) -> Option<Node> {
    let mut cursor = while_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();

            // Look for let_condition which contains the pattern
            if kind == "let_condition" || kind == "let_chain" {
                // Recurse into let_condition to find the pattern
                let mut inner_cursor = child.walk();
                if inner_cursor.goto_first_child() {
                    loop {
                        let inner_child = inner_cursor.node();
                        let inner_kind = inner_child.kind();
                        if inner_kind.contains("pattern")
                            || inner_kind == "tuple_struct_pattern"
                            || inner_kind == "struct_pattern"
                            || inner_kind == "tuple_pattern"
                            || inner_kind == "slice_pattern"
                        {
                            return Some(inner_child);
                        }
                        if !inner_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            // Some tree-sitter grammars put the pattern directly under while_expression
            if kind.contains("pattern")
                || kind == "tuple_struct_pattern"
                || kind == "struct_pattern"
                || kind == "tuple_pattern"
            {
                return Some(child);
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

// Helper functions

/// Extract function parameter names from a function definition node
fn extract_function_parameters(node: Node, source: &[u8]) -> Vec<String> {
    let mut params = Vec::new();
    let mut cursor = node.walk();

    // Find the parameters node (handles various tree-sitter grammar styles)
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();

            // Different languages have different names for parameter lists
            if kind == "parameters"
                || kind == "parameter_list"
                || kind == "formal_parameters"
                || kind == "function_parameters"
            {
                // Now extract individual parameter names
                let mut param_cursor = child.walk();
                if param_cursor.goto_first_child() {
                    loop {
                        let param_child = param_cursor.node();
                        let param_kind = param_child.kind();

                        // Look for parameter/identifier nodes
                        if param_kind == "parameter"
                            || param_kind == "simple_parameter"
                            || param_kind == "formal_parameter"
                        {
                            if let Some(name) = extract_param_name(param_child, source) {
                                params.push(name);
                            }
                        } else if param_kind == "identifier" {
                            if let Ok(name) = param_child.utf8_text(source) {
                                params.push(name.to_string());
                            }
                        }

                        if !param_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                break;
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    params
}

/// Extract parameter name from a parameter node
fn extract_param_name(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();

    // Look for identifier/name child
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();

            if kind == "identifier" || kind == "name" || kind == "pattern" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }

            // Recurse into nested structures (e.g., typed parameters)
            if kind.contains("pattern") || kind == "typed_parameter" {
                if let Some(name) = extract_param_name(child, source) {
                    return Some(name);
                }
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    // If no child identifier found, try the node itself
    if node.kind() == "identifier" {
        return node.utf8_text(source).ok().map(|s| s.to_string());
    }

    None
}

/// The source text of a loop's header — everything before its first
/// statement.
///
/// Used as the text of the header block, so the data-flow pass sees the
/// variables the loop actually reads instead of a synthetic label.
fn loop_header_text(node: Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    let body_start = node
        .named_children(&mut cursor)
        .find(|child| is_inline_statement(child.kind()))
        .map_or(node.end_byte(), |child| child.start_byte());

    let header = source
        .get(node.start_byte()..body_start)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or("for");

    header
        .lines()
        .next()
        .unwrap_or("for")
        .trim()
        .chars()
        .take(100)
        .collect()
}

/// Whether `kind` is a statement that a flat grammar hangs directly off its
/// parent rather than wrapping in a body node.
///
/// Every BSL statement node is named `*_statement`; `preprocessor` is the one
/// exception (`#Если`, `#Область`). Keywords are upper-case, and `expression`,
/// `identifier`, `parameters` and the `*_clause` wrappers do not match either,
/// so this admits exactly the statements.
fn is_inline_statement(kind: &str) -> bool {
    kind.ends_with("_statement") || kind == "preprocessor"
}

/// Whether `node` has any statement hanging directly off it.
fn has_inline_statements(node: Node) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if is_inline_statement(cursor.node().kind()) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn extract_condition(node: Node, source: &[u8]) -> Option<String> {
    // Look for condition child
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            // Common condition node types
            if child.kind().contains("condition")
                || child.kind() == "parenthesized_expression"
                || (child.kind() == "binary_expression"
                    && child.start_position().row == node.start_position().row)
            {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // BSL wraps the condition of `Если`/`Пока` in a plain `expression` child
    // with no field and no parentheses, so none of the shapes above match and
    // every branch used to be labelled with an empty condition.
    find_child_by_kind(node, "expression")
        .and_then(|child| child.utf8_text(source).ok())
        .map(|s| s.to_string())
}

fn extract_variable_name(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "identifier" || child.kind() == "pattern" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract variable name from Go short_var_declaration (x := 5)
fn extract_go_short_var_name(node: Node, source: &[u8]) -> Option<String> {
    // Go short_var_declaration has expression_list on left side
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            // Look for expression_list (left side of :=) or identifier
            if child.kind() == "expression_list" || child.kind() == "identifier" {
                // Get the first identifier from the list
                let mut inner = child.walk();
                if inner.goto_first_child() {
                    loop {
                        let inner_child = inner.node();
                        if inner_child.kind() == "identifier" {
                            return inner_child.utf8_text(source).ok().map(|s| s.to_string());
                        }
                        if !inner.goto_next_sibling() {
                            break;
                        }
                    }
                }
                // If the child itself is an identifier
                if child.kind() == "identifier" {
                    return child.utf8_text(source).ok().map(|s| s.to_string());
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract variable name from Go var_declaration (var x = 5)
fn extract_go_var_name(node: Node, source: &[u8]) -> Option<String> {
    // Go var_spec has identifier as first child
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "identifier" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            // Recurse into var_spec
            if child.kind() == "var_spec" {
                return extract_go_var_name(child, source);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract variable name from Java local_variable_declaration (int x = 5)
fn extract_java_var_name(node: Node, source: &[u8]) -> Option<String> {
    // Java local_variable_declaration has variable_declarator containing identifier
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "variable_declarator" {
                // Get identifier from variable_declarator
                let mut inner = child.walk();
                if inner.goto_first_child() {
                    loop {
                        let inner_child = inner.node();
                        if inner_child.kind() == "identifier" {
                            return inner_child.utf8_text(source).ok().map(|s| s.to_string());
                        }
                        if !inner.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract variable name from Kotlin property_declaration (val x = 5)
fn extract_kotlin_var_name(node: Node, source: &[u8]) -> Option<String> {
    // Kotlin property_declaration has variable_declaration containing simple_identifier
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            // Look for variable_declaration or simple_identifier directly
            if child.kind() == "variable_declaration" {
                let mut inner = child.walk();
                if inner.goto_first_child() {
                    loop {
                        let inner_child = inner.node();
                        if inner_child.kind() == "simple_identifier" {
                            return inner_child.utf8_text(source).ok().map(|s| s.to_string());
                        }
                        if !inner.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            if child.kind() == "simple_identifier" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract variable name from JavaScript/TypeScript declarations (let x = 5, const y = 10, var z)
fn extract_js_var_name(node: Node, source: &[u8]) -> Option<String> {
    // JS/TS lexical_declaration or variable_declaration has variable_declarator
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "variable_declarator" {
                // Get the name child (identifier)
                let mut inner = child.walk();
                if inner.goto_first_child() {
                    let first = inner.node();
                    if first.kind() == "identifier" {
                        return first.utf8_text(source).ok().map(|s| s.to_string());
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Analyze a function and build its CFG
pub fn analyze_function(
    tree: &Tree,
    source: &str,
    file_path: &str,
) -> Result<Vec<ControlFlowGraph>> {
    let source_bytes = source.as_bytes();
    let mut cfgs = Vec::new();

    let mut cursor = tree.walk();
    walk_for_functions(&mut cursor, source_bytes, file_path, &mut cfgs)?;

    Ok(cfgs)
}

fn walk_for_functions(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    file_path: &str,
    cfgs: &mut Vec<ControlFlowGraph>,
) -> Result<()> {
    loop {
        let node = cursor.node();
        let kind = node.kind();

        // Check if this is a function definition
        // Supports: Rust, Python, JavaScript, TypeScript, Go, Java, C#, Kotlin, C, C++
        if matches!(
            kind,
            // Rust
            "function_item"
                // Python, C, C++
                | "function_definition"
                // JavaScript, TypeScript, Go, Kotlin
                | "function_declaration"
                // Python, JavaScript
                | "method_definition"
                // Java, C#
                | "method_declaration"
                // Java, C# constructors
                | "constructor_declaration"
                // BSL: `Процедура` is a separate node from `Функция`, and it
                // is what nearly every 1C event handler is. Without it the
                // whole flow layer was blind to most of a 1C codebase.
                | "procedure_definition"
        ) {
            // Extract function name
            if let Some(name) = extract_function_name_from_node(node, source) {
                match CfgBuilder::build_from_function(&name, file_path, node, source) {
                    Ok(cfg) => cfgs.push(cfg),
                    Err(e) => {
                        tracing::warn!("Failed to build CFG for {}: {}", name, e);
                    }
                }
            }
        }

        // Recurse into children
        if cursor.goto_first_child() {
            walk_for_functions(cursor, source, file_path, cfgs)?;
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    Ok(())
}

fn extract_function_name_from_node(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    cursor.goto_first_child();

    loop {
        let child = cursor.node();
        let kind = child.kind();

        if kind == "identifier"
            || kind == "name"
            || kind == "field_identifier"
            || kind == "property_identifier"
            || kind == "simple_identifier"
        // Kotlin
        {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }

        if kind.contains("declarator") {
            if let Some(name) = extract_function_name_from_node(child, source) {
                return Some(name);
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

/// Check if a name is a type constructor (should not be treated as a variable)
fn is_type_constructor(name: &str) -> bool {
    matches!(
        name,
        // Option variants
        "Some" | "None" |
        // Result variants
        "Ok" | "Err" |
        // Boolean literals
        "true" | "false" |
        // Common type names that might appear in patterns
        "Self"
    )
}

/// Extract pattern bindings from a pattern node (match arm, for loop, if-let, while-let)
///
/// This function recursively walks the pattern AST to find all bound variable names.
/// It filters out type constructors like `Some`, `None`, `Ok`, `Err`.
pub fn extract_pattern_bindings(node: Node, source: &[u8]) -> Vec<String> {
    let mut bindings = Vec::new();
    extract_bindings_recursive(node, source, &mut bindings);
    bindings
}

fn extract_bindings_recursive(node: Node, source: &[u8], bindings: &mut Vec<String>) {
    let kind = node.kind();

    match kind {
        // Direct identifier in pattern position - this is a binding
        "identifier" => {
            if let Ok(name) = node.utf8_text(source) {
                // Filter out type constructors and ensure it starts with lowercase (variable naming convention)
                if !is_type_constructor(name)
                    && !name.is_empty()
                    && (name.starts_with('_')
                        || name.chars().next().is_some_and(|c| c.is_lowercase()))
                {
                    bindings.push(name.to_string());
                }
            }
        }

        // Tuple struct pattern: Some(x) or Ok(value)
        // The first child is the constructor name (skip it), remaining are patterns
        "tuple_struct_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                // Skip the type/constructor name (first identifier)
                let _ = cursor.goto_next_sibling();
                loop {
                    let child = cursor.node();
                    // Recurse into pattern children
                    if child.kind().contains("pattern") || child.kind() == "identifier" {
                        extract_bindings_recursive(child, source, bindings);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Tuple pattern: (a, b)
        "tuple_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    extract_bindings_recursive(child, source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Struct pattern: Point { x, y }
        "struct_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "field_pattern" {
                        // Field pattern can be `name` or `name: pattern`
                        extract_bindings_recursive(child, source, bindings);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Field pattern in struct: x or x: value
        "field_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                // Check if this is shorthand (just identifier) or with pattern
                let mut children: Vec<Node> = Vec::new();
                loop {
                    children.push(cursor.node());
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                if children.len() == 1 {
                    // Shorthand: just `x`
                    extract_bindings_recursive(children[0], source, bindings);
                } else {
                    // Full form: `x: pattern` - recurse into the pattern part
                    for child in children.iter().skip(1) {
                        if child.kind().contains("pattern") || child.kind() == "identifier" {
                            extract_bindings_recursive(*child, source, bindings);
                        }
                    }
                }
            }
        }

        // Slice pattern: [first, rest @ ..]
        "slice_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    extract_bindings_recursive(cursor.node(), source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Reference pattern: &x or &mut x
        "ref_pattern" | "reference_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    extract_bindings_recursive(cursor.node(), source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Or pattern: A | B
        "or_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    extract_bindings_recursive(cursor.node(), source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Rest pattern: ..rest
        "rest_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "identifier" {
                        extract_bindings_recursive(child, source, bindings);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Captured pattern: name @ pattern
        "captured_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    // The identifier before @ is the captured name
                    if child.kind() == "identifier" {
                        if let Ok(name) = child.utf8_text(source) {
                            if !is_type_constructor(name)
                                && !name.is_empty()
                                && (name.starts_with('_')
                                    || name.chars().next().is_some_and(|c| c.is_lowercase()))
                            {
                                bindings.push(name.to_string());
                            }
                        }
                    }
                    // Also recurse into nested patterns
                    if child.kind().contains("pattern") {
                        extract_bindings_recursive(child, source, bindings);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Mut pattern: mut x
        "mut_pattern" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    extract_bindings_recursive(cursor.node(), source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // Generic pattern types - recurse into children
        _ if kind.contains("pattern") => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    extract_bindings_recursive(cursor.node(), source, bindings);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        // For other nodes, don't extract
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_block_creation() {
        let block = BasicBlock {
            id: 0,
            label: "entry".to_string(),
            start_line: 1,
            end_line: 5,
            terminator: Terminator::FallThrough,
            statements: Vec::new(),
            is_entry: true,
            is_exit: false,
        };

        assert_eq!(block.id, 0);
        assert_eq!(block.label, "entry");
        assert!(block.is_entry);
        assert!(!block.is_exit);
    }

    #[test]
    fn test_cfg_creation() {
        let mut cfg = ControlFlowGraph::new("test_function", "test.rs");

        // Add entry block
        let entry = BasicBlock {
            id: 0,
            label: "entry".to_string(),
            start_line: 1,
            end_line: 3,
            terminator: Terminator::Branch {
                condition: "x > 0".to_string(),
            },
            statements: Vec::new(),
            is_entry: true,
            is_exit: false,
        };
        cfg.add_block(entry);

        // Add then block
        let then_block = BasicBlock {
            id: 1,
            label: "then".to_string(),
            start_line: 4,
            end_line: 6,
            terminator: Terminator::Jump,
            statements: Vec::new(),
            is_entry: false,
            is_exit: false,
        };
        cfg.add_block(then_block);

        // Add exit block
        let exit = BasicBlock {
            id: 2,
            label: "exit".to_string(),
            start_line: 7,
            end_line: 8,
            terminator: Terminator::Return,
            statements: Vec::new(),
            is_entry: false,
            is_exit: true,
        };
        cfg.add_block(exit);

        // Add edges
        cfg.add_edge(0, 1, EdgeKind::TrueBranch);
        cfg.add_edge(0, 2, EdgeKind::FalseBranch);
        cfg.add_edge(1, 2, EdgeKind::FallThrough);

        assert_eq!(cfg.blocks.len(), 3);
        assert_eq!(cfg.edges.len(), 3);
        assert_eq!(cfg.entry_block, 0);
        assert_eq!(cfg.exit_blocks, vec![2]);
    }

    #[test]
    fn test_successors_and_predecessors() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        for i in 0..3 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: i == 2,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::FallThrough);
        cfg.add_edge(0, 2, EdgeKind::Jump);
        cfg.add_edge(1, 2, EdgeKind::FallThrough);

        let succs = cfg.successors(0);
        assert_eq!(succs.len(), 2);
        assert!(succs.contains(&1));
        assert!(succs.contains(&2));

        let preds = cfg.predecessors(2);
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&0));
        assert!(preds.contains(&1));
    }

    #[test]
    fn test_dominator_computation() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        // Create diamond pattern: 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3
        for i in 0..4 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: i == 3,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::TrueBranch);
        cfg.add_edge(0, 2, EdgeKind::FalseBranch);
        cfg.add_edge(1, 3, EdgeKind::FallThrough);
        cfg.add_edge(2, 3, EdgeKind::FallThrough);

        cfg.compute_dominators();

        // Block 0 dominates itself
        assert_eq!(cfg.dominators.get(&0), Some(&0));

        // Block 0 is the immediate dominator of 1, 2, and 3
        assert_eq!(cfg.dominators.get(&1), Some(&0));
        assert_eq!(cfg.dominators.get(&2), Some(&0));
        assert_eq!(cfg.dominators.get(&3), Some(&0));
    }

    #[test]
    fn test_unreachable_block_detection() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        // Create: 0 -> 1, 2 (unreachable)
        for i in 0..3 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: i == 1,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::FallThrough);
        // Block 2 has no incoming edges

        cfg.find_unreachable_blocks();

        assert_eq!(cfg.unreachable_blocks.len(), 1);
        assert!(cfg.unreachable_blocks.contains(&2));
    }

    #[test]
    fn test_loop_detection() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        // Create: 0 -> 1 -> 2 -> 1 (loop back)
        for i in 0..3 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: false,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::FallThrough);
        cfg.add_edge(1, 2, EdgeKind::TrueBranch);
        cfg.add_edge(2, 1, EdgeKind::LoopBack); // Back edge

        cfg.compute_dominators();

        let loops = cfg.find_loops();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0], (2, 1)); // Back edge from 2 to 1
    }

    #[test]
    fn test_dominates() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        for i in 0..4 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: i == 3,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::FallThrough);
        cfg.add_edge(1, 2, EdgeKind::FallThrough);
        cfg.add_edge(2, 3, EdgeKind::FallThrough);

        cfg.compute_dominators();

        // 0 dominates all blocks
        assert!(cfg.dominates(0, 0));
        assert!(cfg.dominates(0, 1));
        assert!(cfg.dominates(0, 2));
        assert!(cfg.dominates(0, 3));

        // 1 dominates 2, 3 but not 0
        assert!(cfg.dominates(1, 1));
        assert!(cfg.dominates(1, 2));
        assert!(cfg.dominates(1, 3));
        assert!(!cfg.dominates(1, 0));
    }

    #[test]
    fn test_cfg_builder() {
        let mut builder = CfgBuilder::new("test_func", "test.rs");

        let entry = builder.create_block("entry");
        builder.set_entry(entry);

        builder.add_statement(
            entry,
            Statement {
                line: 1,
                kind: StatementKind::Assignment {
                    variable: "x".to_string(),
                },
                text: "let x = 5;".to_string(),
            },
        );

        let exit = builder.create_block("exit");
        builder.set_exit(exit);
        builder.set_terminator(exit, Terminator::Return);

        builder.add_edge(entry, exit, EdgeKind::FallThrough);

        let cfg = builder.build();

        assert_eq!(cfg.blocks.len(), 2);
        assert_eq!(cfg.entry_block, 0);
        assert_eq!(cfg.exit_blocks, vec![1]);
    }

    #[test]
    fn test_loop_stack() {
        let mut builder = CfgBuilder::new("test", "test.rs");

        let header = builder.create_block("loop_header");
        let exit = builder.create_block("loop_exit");

        builder.push_loop(header, exit);

        assert_eq!(builder.current_loop_header(), Some(header));
        assert_eq!(builder.current_loop_exit(), Some(exit));

        builder.pop_loop();

        assert_eq!(builder.current_loop_header(), None);
        assert_eq!(builder.current_loop_exit(), None);
    }

    #[test]
    fn test_to_markdown() {
        let mut cfg = ControlFlowGraph::new("test_func", "test.rs");

        cfg.add_block(BasicBlock {
            id: 0,
            label: "entry".to_string(),
            start_line: 1,
            end_line: 2,
            terminator: Terminator::FallThrough,
            statements: Vec::new(),
            is_entry: true,
            is_exit: false,
        });

        cfg.add_block(BasicBlock {
            id: 1,
            label: "exit".to_string(),
            start_line: 3,
            end_line: 4,
            terminator: Terminator::Return,
            statements: Vec::new(),
            is_entry: false,
            is_exit: true,
        });

        cfg.add_edge(0, 1, EdgeKind::FallThrough);

        let md = cfg.to_markdown();

        assert!(md.contains("Control Flow Graph: `test_func`"));
        assert!(md.contains("test.rs"));
        assert!(md.contains("Block 0"));
        assert!(md.contains("Block 1"));
        assert!(md.contains("ENTRY"));
        assert!(md.contains("EXIT"));
    }

    #[test]
    fn test_to_dot() {
        let mut cfg = ControlFlowGraph::new("test_func", "test.rs");

        cfg.add_block(BasicBlock {
            id: 0,
            label: "entry".to_string(),
            start_line: 1,
            end_line: 2,
            terminator: Terminator::Branch {
                condition: "x > 0".to_string(),
            },
            statements: Vec::new(),
            is_entry: true,
            is_exit: false,
        });

        cfg.add_block(BasicBlock {
            id: 1,
            label: "exit".to_string(),
            start_line: 3,
            end_line: 4,
            terminator: Terminator::Return,
            statements: Vec::new(),
            is_entry: false,
            is_exit: true,
        });

        cfg.add_edge(0, 1, EdgeKind::FallThrough);

        let dot = cfg.to_dot();

        assert!(dot.contains("digraph CFG"));
        assert!(dot.contains("0 -> 1"));
    }

    #[test]
    fn test_statement_kinds() {
        let assignment = StatementKind::Assignment {
            variable: "x".to_string(),
        };
        let call = StatementKind::Call {
            function: "foo".to_string(),
        };

        assert_ne!(assignment, call);
        assert_eq!(StatementKind::Return, StatementKind::Return);
    }

    #[test]
    fn test_edge_kinds() {
        assert_ne!(EdgeKind::TrueBranch, EdgeKind::FalseBranch);
        assert_ne!(EdgeKind::FallThrough, EdgeKind::Jump);
        assert_eq!(EdgeKind::LoopBack, EdgeKind::LoopBack);
    }

    #[test]
    fn test_terminator_types() {
        let branch = Terminator::Branch {
            condition: "x > 0".to_string(),
        };
        let jump = Terminator::Jump;

        assert_ne!(branch, jump);
        assert_eq!(Terminator::Return, Terminator::Return);
    }

    #[test]
    fn test_get_loop_blocks() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        // Create loop: 0 -> 1 -> 2 -> 3 -> 1
        for i in 0..4 {
            cfg.add_block(BasicBlock {
                id: i,
                label: format!("block_{}", i),
                start_line: i + 1,
                end_line: i + 1,
                terminator: Terminator::FallThrough,
                statements: Vec::new(),
                is_entry: i == 0,
                is_exit: false,
            });
        }

        cfg.add_edge(0, 1, EdgeKind::FallThrough);
        cfg.add_edge(1, 2, EdgeKind::TrueBranch);
        cfg.add_edge(2, 3, EdgeKind::FallThrough);
        cfg.add_edge(3, 1, EdgeKind::LoopBack);

        cfg.compute_dominators();

        let loop_blocks = cfg.get_loop_blocks(1, 3);

        // Loop should contain blocks 1, 2, 3
        assert!(loop_blocks.contains(&1));
        assert!(loop_blocks.contains(&2));
        assert!(loop_blocks.contains(&3));
        assert!(!loop_blocks.contains(&0)); // Entry not in loop
    }

    #[test]
    fn test_multiple_exit_blocks() {
        let mut cfg = ControlFlowGraph::new("test", "test.rs");

        // Function with early return
        cfg.add_block(BasicBlock {
            id: 0,
            label: "entry".to_string(),
            start_line: 1,
            end_line: 2,
            terminator: Terminator::Branch {
                condition: "x > 0".to_string(),
            },
            statements: Vec::new(),
            is_entry: true,
            is_exit: false,
        });

        cfg.add_block(BasicBlock {
            id: 1,
            label: "early_return".to_string(),
            start_line: 3,
            end_line: 4,
            terminator: Terminator::Return,
            statements: Vec::new(),
            is_entry: false,
            is_exit: true,
        });

        cfg.add_block(BasicBlock {
            id: 2,
            label: "normal_return".to_string(),
            start_line: 5,
            end_line: 6,
            terminator: Terminator::Return,
            statements: Vec::new(),
            is_entry: false,
            is_exit: true,
        });

        cfg.add_edge(0, 1, EdgeKind::TrueBranch);
        cfg.add_edge(0, 2, EdgeKind::FalseBranch);

        assert_eq!(cfg.exit_blocks.len(), 2);
        assert!(cfg.exit_blocks.contains(&1));
        assert!(cfg.exit_blocks.contains(&2));
    }

    // Tests for pattern binding extraction (Phase A1)

    #[test]
    fn test_is_type_constructor() {
        assert!(is_type_constructor("Some"));
        assert!(is_type_constructor("None"));
        assert!(is_type_constructor("Ok"));
        assert!(is_type_constructor("Err"));
        assert!(is_type_constructor("true"));
        assert!(is_type_constructor("false"));
        assert!(!is_type_constructor("value"));
        assert!(!is_type_constructor("x"));
        assert!(!is_type_constructor("result"));
    }

    #[test]
    fn test_pattern_binding_statement_kind() {
        let stmt = Statement {
            line: 1,
            kind: StatementKind::PatternBinding {
                variables: vec!["x".to_string(), "y".to_string()],
            },
            text: "Some((x, y))".to_string(),
        };

        match stmt.kind {
            StatementKind::PatternBinding { variables } => {
                assert_eq!(variables.len(), 2);
                assert!(variables.contains(&"x".to_string()));
                assert!(variables.contains(&"y".to_string()));
            }
            _ => panic!("Expected PatternBinding"),
        }
    }

    #[test]
    fn test_extract_pattern_bindings_simple_identifier() {
        // Test with tree-sitter parsing a simple pattern
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        // Use a match expression to get a pattern node
        let source = r#"fn test() { match x { value => {} } }"#;
        let tree = parser.parse(source, None).unwrap();

        // Find the match arm pattern
        let root = tree.root_node();
        let mut found_binding = false;

        fn find_pattern(node: tree_sitter::Node, source: &[u8]) -> Option<Vec<String>> {
            if node.kind() == "match_arm" {
                // The first child of match_arm is the pattern
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind().contains("pattern") || child.kind() == "identifier" {
                            return Some(super::extract_pattern_bindings(child, source));
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if let Some(result) = find_pattern(cursor.node(), source) {
                        return Some(result);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            None
        }

        if let Some(bindings) = find_pattern(root, source.as_bytes()) {
            assert!(
                bindings.contains(&"value".to_string()),
                "Should extract 'value' binding, got {:?}",
                bindings
            );
            found_binding = true;
        }

        assert!(found_binding, "Should find a pattern binding");
    }

    #[test]
    fn test_extract_pattern_bindings_tuple_struct_some() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let source = r#"fn test() { match x { Some(value) => {} _ => {} } }"#;
        let tree = parser.parse(source, None).unwrap();

        fn find_first_match_arm_pattern(
            node: tree_sitter::Node,
            source: &[u8],
        ) -> Option<Vec<String>> {
            if node.kind() == "match_arm" {
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind().contains("pattern") {
                            return Some(super::extract_pattern_bindings(child, source));
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if let Some(result) = find_first_match_arm_pattern(cursor.node(), source) {
                        return Some(result);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            None
        }

        let bindings = find_first_match_arm_pattern(tree.root_node(), source.as_bytes())
            .expect("Should find pattern");

        // Should extract 'value' but NOT 'Some'
        assert!(
            bindings.contains(&"value".to_string()),
            "Should extract 'value', got {:?}",
            bindings
        );
        assert!(
            !bindings.iter().any(|s| s == "Some"),
            "Should NOT extract 'Some' constructor, got {:?}",
            bindings
        );
    }

    #[test]
    fn test_extract_pattern_bindings_nested_result_option() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let source = r#"fn test() { match x { Ok(Some(inner)) => {} _ => {} } }"#;
        let tree = parser.parse(source, None).unwrap();

        fn find_first_match_arm_pattern(
            node: tree_sitter::Node,
            source: &[u8],
        ) -> Option<Vec<String>> {
            if node.kind() == "match_arm" {
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind().contains("pattern") {
                            return Some(super::extract_pattern_bindings(child, source));
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if let Some(result) = find_first_match_arm_pattern(cursor.node(), source) {
                        return Some(result);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            None
        }

        let bindings = find_first_match_arm_pattern(tree.root_node(), source.as_bytes())
            .expect("Should find pattern");

        // Should extract 'inner' but NOT 'Ok' or 'Some'
        assert!(
            bindings.contains(&"inner".to_string()),
            "Should extract 'inner', got {:?}",
            bindings
        );
        assert!(
            !bindings.iter().any(|s| s == "Ok"),
            "Should NOT extract 'Ok' constructor"
        );
        assert!(
            !bindings.iter().any(|s| s == "Some"),
            "Should NOT extract 'Some' constructor"
        );
    }

    #[test]
    fn test_extract_pattern_bindings_for_loop() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let source = r#"fn test() { for item in items { } }"#;
        let tree = parser.parse(source, None).unwrap();

        fn find_for_pattern(node: tree_sitter::Node, source: &[u8]) -> Option<Vec<String>> {
            if node.kind() == "for_expression" {
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        // In Rust tree-sitter, the pattern is the first meaningful child after 'for'
                        if child.kind().contains("pattern") || child.kind() == "identifier" {
                            return Some(super::extract_pattern_bindings(child, source));
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if let Some(result) = find_for_pattern(cursor.node(), source) {
                        return Some(result);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            None
        }

        let bindings = find_for_pattern(tree.root_node(), source.as_bytes())
            .expect("Should find for loop pattern");

        assert!(
            bindings.contains(&"item".to_string()),
            "Should extract 'item', got {:?}",
            bindings
        );
    }

    #[test]
    fn test_extract_pattern_bindings_tuple_destructure() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let source = r#"fn test() { for (a, b) in pairs { } }"#;
        let tree = parser.parse(source, None).unwrap();

        fn find_for_pattern(node: tree_sitter::Node, source: &[u8]) -> Option<Vec<String>> {
            if node.kind() == "for_expression" {
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind().contains("pattern") {
                            return Some(super::extract_pattern_bindings(child, source));
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if let Some(result) = find_for_pattern(cursor.node(), source) {
                        return Some(result);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            None
        }

        let bindings = find_for_pattern(tree.root_node(), source.as_bytes())
            .expect("Should find for loop pattern");

        assert!(
            bindings.contains(&"a".to_string()),
            "Should extract 'a', got {:?}",
            bindings
        );
        assert!(
            bindings.contains(&"b".to_string()),
            "Should extract 'b', got {:?}",
            bindings
        );
    }

    // ===================================================================================
    // Multi-Language CFG Support Tests (Go, Java, C#, Kotlin)
    // ===================================================================================

    #[test]
    fn test_go_switch_statement_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();

        let source = r#"
package main

func test(x int) int {
    switch x {
    case 1:
        return 10
    case 2:
        return 20
    default:
        return 0
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "test.go").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for Go function");
        let cfg = &cfgs[0];

        // Switch should create multiple branches
        assert!(
            cfg.blocks.len() >= 3,
            "Switch should create multiple blocks"
        );
    }

    #[test]
    fn test_go_for_statement_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();

        let source = r#"
package main

func test() {
    for i := 0; i < 10; i++ {
        println(i)
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "test.go").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for Go for loop");
        let cfg = &cfgs[0];

        // For loop should create header, body, and exit blocks
        assert!(
            cfg.blocks.len() >= 3,
            "For loop should create header, body, and exit blocks"
        );
    }

    #[test]
    fn test_go_defer_statement_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();

        let source = r#"
package main

func test() {
    defer cleanup()
    doWork()
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "test.go").unwrap();

        assert!(
            !cfgs.is_empty(),
            "Should build CFG for Go function with defer"
        );
    }

    #[test]
    fn test_java_try_catch_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void test() {
        try {
            riskyOperation();
        } catch (Exception e) {
            handleError(e);
        } finally {
            cleanup();
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.java").unwrap();

        assert!(
            !cfgs.is_empty(),
            "Should build CFG for Java method with try-catch"
        );
        let cfg = &cfgs[0];

        // Try-catch should create multiple blocks for try, catch, and finally
        assert!(
            cfg.blocks.len() >= 3,
            "Try-catch should create blocks for try, catch, finally"
        );
    }

    #[test]
    fn test_java_enhanced_for_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void test(List<String> items) {
        for (String item : items) {
            process(item);
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.java").unwrap();

        assert!(
            !cfgs.is_empty(),
            "Should build CFG for Java enhanced for loop"
        );
        let cfg = &cfgs[0];

        // Enhanced for loop should create header, body, and exit blocks
        assert!(
            cfg.blocks.len() >= 3,
            "Enhanced for should create loop blocks"
        );
    }

    #[test]
    fn test_java_switch_expression_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void test(int x) {
        switch (x) {
            case 1:
                System.out.println("one");
                break;
            case 2:
                System.out.println("two");
                break;
            default:
                System.out.println("other");
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.java").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for Java switch");
        let cfg = &cfgs[0];

        // Switch should create multiple case blocks
        assert!(
            cfg.blocks.len() >= 3,
            "Switch should create multiple blocks"
        );
    }

    #[test]
    fn test_csharp_foreach_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void Test(List<string> items) {
        foreach (var item in items) {
            Process(item);
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.cs").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for C# foreach");
        let cfg = &cfgs[0];

        // Foreach should create loop blocks
        assert!(cfg.blocks.len() >= 3, "Foreach should create loop blocks");
    }

    #[test]
    fn test_csharp_using_statement_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void Test() {
        using (var file = OpenFile()) {
            file.Write("test");
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.cs").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for C# using statement");
    }

    #[test]
    fn test_csharp_switch_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void Test(int x) {
        switch (x) {
            case 1:
                Console.WriteLine("one");
                break;
            case 2:
                Console.WriteLine("two");
                break;
            default:
                Console.WriteLine("other");
                break;
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.cs").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for C# switch");
        let cfg = &cfgs[0];

        // Switch should create multiple case blocks
        assert!(
            cfg.blocks.len() >= 3,
            "Switch should create multiple blocks"
        );
    }

    // Debug helper to dump AST nodes
    fn dump_ast(node: tree_sitter::Node, depth: usize, source: &str) {
        let indent = "  ".repeat(depth);
        let text = node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>()
            .replace('\n', "\\n");
        eprintln!(
            "{}{}[{}] '{}'",
            indent,
            node.kind(),
            node.start_position().row,
            text
        );
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                dump_ast(cursor.node(), depth + 1, source);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    #[test]
    fn test_go_ast_dump() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let source = r#"
package main
func test(x int) int {
    switch x {
    case 1:
        return 10
    default:
        return 0
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        dump_ast(tree.root_node(), 0, source);
    }

    #[test]
    fn test_java_ast_dump() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let source = r#"
class Test {
    void test(int x) {
        switch (x) {
            case 1:
                System.out.println("one");
                break;
            default:
                System.out.println("other");
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        dump_ast(tree.root_node(), 0, source);
    }

    #[test]
    fn test_csharp_ast_dump() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        let source = r#"
class Test {
    void Test(int x) {
        switch (x) {
            case 1:
                Console.WriteLine("one");
                break;
            default:
                Console.WriteLine("other");
                break;
        }
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        dump_ast(tree.root_node(), 0, source);
    }

    #[test]
    fn test_kotlin_ast_dump() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_sg::LANGUAGE.into())
            .unwrap();
        let source = r#"
fun test(x: Int): String {
    return when (x) {
        1 -> "one"
        2 -> "two"
        else -> "other"
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        dump_ast(tree.root_node(), 0, source);
    }

    #[test]
    fn test_kotlin_when_expression_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_sg::LANGUAGE.into())
            .unwrap();

        let source = r#"
fun test(x: Int): String {
    return when (x) {
        1 -> "one"
        2 -> "two"
        else -> "other"
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.kt").unwrap();

        assert!(
            !cfgs.is_empty(),
            "Should build CFG for Kotlin when expression"
        );
        let cfg = &cfgs[0];

        // When should create multiple branches
        assert!(cfg.blocks.len() >= 3, "When should create multiple blocks");
    }

    #[test]
    fn test_kotlin_try_catch_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_sg::LANGUAGE.into())
            .unwrap();

        let source = r#"
fun test() {
    try {
        riskyOperation()
    } catch (e: Exception) {
        handleError(e)
    } finally {
        cleanup()
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.kt").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for Kotlin try-catch");
        let cfg = &cfgs[0];

        // Try-catch should create multiple blocks
        assert!(
            cfg.blocks.len() >= 3,
            "Try-catch should create multiple blocks"
        );
    }

    /// BSL was invisible to the whole flow layer. `Процедура` is a node type
    /// of its own that the function walk did not list, and the grammar has no
    /// body node at all — statements hang directly off the definition — so
    /// even `Функция` failed with "no function body". Every call answered
    /// `Function 'X' not found`, which reads as "there is no such function"
    /// while meaning "this language is unsupported".
    #[test]
    fn test_bsl_procedure_and_function_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bsl::LANGUAGE.into())
            .unwrap();

        let source = "Процедура Приветствие(Отказ)\n\tЕсли Отказ Тогда\n\t\tВозврат;\n\tКонецЕсли;\n\tСообщить(\"привет\");\nКонецПроцедуры\n\nФункция Сумма(Список)\n\tИтог = 0;\n\tДля Каждого Элемент Из Список Цикл\n\t\tИтог = Итог + Элемент;\n\tКонецЦикла;\n\tВозврат Итог;\nКонецФункции\n";

        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Module.bsl").unwrap();

        let names: Vec<&str> = cfgs.iter().map(|c| c.function_name.as_str()).collect();
        assert!(
            names.contains(&"Приветствие"),
            "a `Процедура` must reach the flow layer, got {names:?}"
        );
        assert!(
            names.contains(&"Сумма"),
            "a `Функция` must reach the flow layer, got {names:?}"
        );

        // The procedure branches, so its guarded `Возврат` is its own block.
        let proc = cfgs
            .iter()
            .find(|c| c.function_name == "Приветствие")
            .unwrap();
        assert!(
            proc.blocks.len() >= 3,
            "`Если` must produce branches, got {} block(s)",
            proc.blocks.len()
        );
        assert!(
            proc.blocks
                .values()
                .any(|b| b.statements.iter().any(|s| s.text.contains("Сообщить"))),
            "the statements after `КонецЕсли` are missing from the graph"
        );

        // The loop must close, and its body statement must be inside it.
        let func = cfgs.iter().find(|c| c.function_name == "Сумма").unwrap();
        assert!(
            func.edges.iter().any(|e| e.kind == EdgeKind::LoopBack),
            "`Для Каждого` must produce a loop back edge"
        );
        assert!(
            func.blocks.values().any(|b| b.statements.iter().any(|s| {
                matches!(&s.kind, StatementKind::Assignment { variable } if variable == "Итог")
            })),
            "`Итог = Итог + Элемент` must be recorded as an assignment"
        );
    }

    /// `ИначеЕсли` is everywhere in 1C code. Its statements used to be dropped
    /// from the graph entirely rather than appearing on a branch of their own.
    #[test]
    fn test_bsl_elseif_chain_is_a_branch() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bsl::LANGUAGE.into())
            .unwrap();

        let source = "Процедура Выбор(Режим)\n\tЕсли Режим = 1 Тогда\n\t\tПервый();\n\tИначеЕсли Режим = 2 Тогда\n\t\tВторой();\n\tИначе\n\t\tТретий();\n\tКонецЕсли;\nКонецПроцедуры\n";

        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Module.bsl").unwrap();
        let cfg = cfgs.first().expect("a CFG for `Выбор`");

        for expected in ["Первый", "Второй", "Третий"] {
            assert!(
                cfg.blocks
                    .values()
                    .any(|b| b.statements.iter().any(|s| s.text.contains(expected))),
                "branch calling {expected} is missing from the graph"
            );
        }
        // The `ИначеЕсли` test is a branch of its own, not a fall-through.
        assert!(
            cfg.blocks.values().any(|b| b
                .statements
                .iter()
                .any(|s| s.text.contains("else if Режим = 2"))),
            "the `ИначеЕсли` condition was not recorded as a branch"
        );
    }

    /// `Попытка`/`Исключение` have no clause wrappers: both halves are direct
    /// children of `try_statement`, split only by the keyword token. Looking
    /// for a `catch_clause` that cannot exist dropped both.
    #[test]
    fn test_bsl_try_except_splits_on_keyword() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bsl::LANGUAGE.into())
            .unwrap();

        let source = "Процедура Риск()\n\tПопытка\n\t\tОпасно();\n\tИсключение\n\t\tОбработать();\n\tКонецПопытки;\nКонецПроцедуры\n";

        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Module.bsl").unwrap();
        let cfg = cfgs.first().expect("a CFG for `Риск`");

        let block_with = |needle: &str| {
            cfg.blocks
                .iter()
                .find(|(_, b)| b.statements.iter().any(|s| s.text.contains(needle)))
                .map(|(id, _)| *id)
        };
        let guarded = block_with("Опасно").expect("the guarded statement is missing");
        let handler = block_with("Обработать").expect("the handler statement is missing");
        assert_ne!(
            guarded, handler,
            "guarded and handler statements must not share a block"
        );
        assert!(
            cfg.edges
                .iter()
                .any(|e| e.to == handler && e.kind == EdgeKind::Jump),
            "the handler must be reached by an exception edge"
        );
    }

    #[test]
    fn test_do_while_loop_cfg() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let source = r#"
class Test {
    void test() {
        int i = 0;
        do {
            i++;
        } while (i < 10);
    }
}
"#;
        let tree = parser.parse(source, None).unwrap();
        let cfgs = analyze_function(&tree, source, "Test.java").unwrap();

        assert!(!cfgs.is_empty(), "Should build CFG for do-while loop");
        let cfg = &cfgs[0];

        // Do-while should create body and condition blocks
        assert!(cfg.blocks.len() >= 3, "Do-while should create loop blocks");
    }
}
