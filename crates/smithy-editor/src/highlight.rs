//! Multi-language syntax highlighting with tree-sitter.
//!
//! Rust, Python, JavaScript, TypeScript, TSX, Go, C, C++, JSON, HTML and CSS —
//! [`SupportedLanguage`] is the list, and `all()` is what the coverage test
//! walks so a language added there cannot go unchecked.
//!
//! Parsers and queries are built when a language is first used. Parsing is
//! genuinely incremental: [`SyntaxHighlighter::update`] hands tree-sitter the
//! previous tree along with the edit, and [`crate::syntax_styling`] is what
//! calls it. That claim used to be here and be false — `update` had no caller
//! at all, and every keystroke re-parsed the whole file.

use std::path::Path;

use lapce_xi_rope::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

use crate::error::HighlightError;

/// Supported languages for syntax highlighting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Json,
    Html,
    Css,
}

impl SupportedLanguage {
    /// Get the tree-sitter Language for this supported language
    pub fn tree_sitter_language(&self) -> Language {
        match self {
            SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
            SupportedLanguage::Python => tree_sitter_python::LANGUAGE.into(),
            SupportedLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            SupportedLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            SupportedLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
            SupportedLanguage::C => tree_sitter_c::LANGUAGE.into(),
            SupportedLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            SupportedLanguage::Json => tree_sitter_json::LANGUAGE.into(),
            SupportedLanguage::Html => tree_sitter_html::LANGUAGE.into(),
            SupportedLanguage::Css => tree_sitter_css::LANGUAGE.into(),
        }
    }

    /// Get the highlight query for this language
    pub fn highlight_query(&self) -> &'static str {
        match self {
            SupportedLanguage::Rust => RUST_HIGHLIGHTS_QUERY,
            SupportedLanguage::Python => PYTHON_HIGHLIGHTS_QUERY,
            SupportedLanguage::JavaScript => JAVASCRIPT_HIGHLIGHTS_QUERY,
            SupportedLanguage::TypeScript => TYPESCRIPT_HIGHLIGHTS_QUERY,
            SupportedLanguage::Tsx => TSX_HIGHLIGHTS_QUERY,
            SupportedLanguage::Go => GO_HIGHLIGHTS_QUERY,
            SupportedLanguage::C => C_HIGHLIGHTS_QUERY,
            SupportedLanguage::Cpp => CPP_HIGHLIGHTS_QUERY,
            SupportedLanguage::Json => JSON_HIGHLIGHTS_QUERY,
            SupportedLanguage::Html => HTML_HIGHLIGHTS_QUERY,
            SupportedLanguage::Css => CSS_HIGHLIGHTS_QUERY,
        }
    }

    /// Get the language ID string
    pub fn id(&self) -> &'static str {
        match self {
            SupportedLanguage::Rust => "rust",
            SupportedLanguage::Python => "python",
            SupportedLanguage::JavaScript => "javascript",
            SupportedLanguage::TypeScript => "typescript",
            SupportedLanguage::Tsx => "tsx",
            SupportedLanguage::Go => "go",
            SupportedLanguage::C => "c",
            SupportedLanguage::Cpp => "cpp",
            SupportedLanguage::Json => "json",
            SupportedLanguage::Html => "html",
            SupportedLanguage::Css => "css",
        }
    }

    /// Detect language from file extension
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(SupportedLanguage::Rust),
            "py" | "pyw" | "pyi" => Some(SupportedLanguage::Python),
            "js" | "mjs" | "cjs" => Some(SupportedLanguage::JavaScript),
            "ts" | "mts" | "cts" => Some(SupportedLanguage::TypeScript),
            "tsx" => Some(SupportedLanguage::Tsx),
            "jsx" => Some(SupportedLanguage::JavaScript), // JSX uses JS grammar
            "go" => Some(SupportedLanguage::Go),
            "c" | "h" => Some(SupportedLanguage::C),
            "cpp" | "cxx" | "cc" | "hpp" | "hxx" | "hh" => Some(SupportedLanguage::Cpp),
            "json" => Some(SupportedLanguage::Json),
            "html" | "htm" => Some(SupportedLanguage::Html),
            "css" => Some(SupportedLanguage::Css),
            _ => None,
        }
    }

    /// Every language this crate claims to support.
    ///
    /// `#[cfg(test)]` because its only caller is
    /// `every_supported_language_actually_highlights_something`, and that is the
    /// point of it: a language added to the enum is covered by that test without
    /// anyone remembering to extend a list. Exposing it publicly with no
    /// production caller is exactly the shape this crate has just finished
    /// clearing out.
    #[cfg(test)]
    pub fn all() -> &'static [SupportedLanguage] {
        &[
            SupportedLanguage::Rust,
            SupportedLanguage::Python,
            SupportedLanguage::JavaScript,
            SupportedLanguage::TypeScript,
            SupportedLanguage::Tsx,
            SupportedLanguage::Go,
            SupportedLanguage::C,
            SupportedLanguage::Cpp,
            SupportedLanguage::Json,
            SupportedLanguage::Html,
            SupportedLanguage::Css,
        ]
    }

    /// Detect language from file path
    pub fn from_path(path: &Path) -> Option<Self> {
        // First check special filenames
        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            match filename {
                "tsconfig.json" | "package.json" | "package-lock.json" => {
                    return Some(SupportedLanguage::Json)
                }
                "Makefile" | "makefile" => return None, // TODO: Add makefile support
                "Dockerfile" => return None,            // TODO: Add dockerfile support
                _ => {}
            }
        }

        // Then check extension
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_extension)
    }
}

/// Style for highlighted text spans
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightStyle {
    Keyword,
    String,
    Comment,
    Type,
    Identifier,
    Number,
    Operator,
    Punctuation,
    Function,
    Variable,
    Constant,
    Attribute,
    Namespace,
}

/// A range of text with a highlight style
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
    pub style: HighlightStyle,
}

impl HighlightRange {
    /// Create a new highlight range
    pub fn new(start: usize, end: usize, style: HighlightStyle) -> Self {
        Self { start, end, style }
    }
}

/// Syntax highlighter using Tree-sitter for incremental parsing
pub struct SyntaxHighlighter {
    parser: Parser,
    tree: Option<Tree>,
    language_id: Option<String>,
    highlights: Vec<HighlightRange>,
    query: Option<Query>,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            tree: None,
            language_id: None,
            highlights: Vec::new(),
            query: None,
        }
    }

    /// Set the language using a SupportedLanguage enum
    pub fn set_language_enum(&mut self, lang: SupportedLanguage) -> Result<(), HighlightError> {
        let language = lang.tree_sitter_language();

        self.parser
            .set_language(&language)
            .map_err(|e| HighlightError::ParseError(format!("Failed to set language: {}", e)))?;

        // Set up the highlight query for the language
        self.query = Some(Self::create_highlight_query(
            &language,
            lang.highlight_query(),
        )?);
        self.language_id = Some(lang.id().to_string());
        self.tree = None;
        self.highlights.clear();

        Ok(())
    }

    /// Set language from a file path (auto-detection)
    pub fn set_language_from_path(&mut self, path: &Path) -> Result<(), HighlightError> {
        let supported = SupportedLanguage::from_path(path).ok_or_else(|| {
            HighlightError::UnsupportedLanguage(path.to_string_lossy().to_string())
        })?;

        self.set_language_enum(supported)
    }

    /// Create a highlight query for the given language
    fn create_highlight_query(
        language: &Language,
        query_source: &str,
    ) -> Result<Query, HighlightError> {
        Query::new(language, query_source)
            .map_err(|e| HighlightError::ParseError(format!("Failed to create query: {}", e)))
    }

    /// Parse `text` and generate highlights, **without copying it**.
    ///
    /// This used to take a `ropey::Rope` and do `text.chars().collect::<String>()`
    /// — a char-by-char rebuild of the entire file, on every keystroke, and the
    /// third copy in a chain: floem's rope to `String` at the call site, `String`
    /// to `ropey::Rope`, then `ropey::Rope` back to `String` here.
    ///
    /// `Parser::parse_with_options` takes a callback returning the bytes at an
    /// offset — it was `parse_with` before tree-sitter 0.26, and the callback shape
    /// is unchanged — while `lapce_xi_rope`'s `iter_chunks` yields `&str` borrowed
    /// straight from the rope's own B-tree leaves. So the parser reads floem's
    /// buffer in place and nothing is allocated.
    ///
    /// Returns `Err(HighlightError)` if no language is set or the parse fails.
    pub fn parse(&mut self, text: &Rope) -> Result<(), HighlightError> {
        if self.language_id.is_none() {
            return Err(HighlightError::ParseError(
                "No language set for highlighting".to_string(),
            ));
        }

        let len = text.len();
        let mut read = |byte_offset: usize, _pos: tree_sitter::Point| -> &[u8] {
            if byte_offset >= len {
                return &[];
            }
            text.iter_chunks(byte_offset..len)
                .next()
                .map(str::as_bytes)
                .unwrap_or(&[])
        };

        let tree = self
            .parser
            .parse_with_options(&mut read, None, None)
            .ok_or_else(|| HighlightError::ParseError("Failed to parse text".to_string()))?;

        self.tree = Some(tree);
        self.compute_highlights(text)?;

        Ok(())
    }

    /// Update the syntax tree incrementally after an edit
    ///
    /// # Arguments
    /// * `text` - The rope containing the updated text
    /// * `start_byte` - The byte offset where the edit started
    /// * `old_end_byte` - The byte offset where the old text ended
    /// * `new_end_byte` - The byte offset where the new text ends
    /// * `start_position` - The (row, column) position where the edit started
    /// * `old_end_position` - The (row, column) position where the old text ended
    /// * `new_end_position` - The (row, column) position where the new text ends
    ///
    /// # Returns
    /// * `Ok(())` if update succeeded
    /// * `Err(HighlightError)` if update failed
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        text: &Rope,
        start_byte: usize,
        old_end_byte: usize,
        new_end_byte: usize,
        start_position: (usize, usize),
        old_end_position: (usize, usize),
        new_end_position: (usize, usize),
    ) -> Result<(), HighlightError> {
        if let Some(tree) = &mut self.tree {
            // Create the input edit for tree-sitter
            let edit = tree_sitter::InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position: tree_sitter::Point {
                    row: start_position.0,
                    column: start_position.1,
                },
                old_end_position: tree_sitter::Point {
                    row: old_end_position.0,
                    column: old_end_position.1,
                },
                new_end_position: tree_sitter::Point {
                    row: new_end_position.0,
                    column: new_end_position.1,
                },
            };

            tree.edit(&edit);

            // Re-parse with the old tree for incremental parsing. Same zero-copy
            // read as the full parse above.
            let len = text.len();
            let mut read = |byte_offset: usize, _pos: tree_sitter::Point| -> &[u8] {
                if byte_offset >= len {
                    return &[];
                }
                text.iter_chunks(byte_offset..len)
                    .next()
                    .map(str::as_bytes)
                    .unwrap_or(&[])
            };
            let new_tree = self
                .parser
                .parse_with_options(&mut read, Some(tree), None)
                .ok_or_else(|| HighlightError::ParseError("Failed to re-parse text".to_string()))?;

            self.tree = Some(new_tree);
            self.compute_highlights(text)?;
        } else {
            // No existing tree, do a full parse
            self.parse(text)?;
        }

        Ok(())
    }

    /// Get all highlights
    pub fn highlights(&self) -> &[HighlightRange] {
        &self.highlights
    }

    /// Check if a language is set
    pub fn has_language(&self) -> bool {
        self.language_id.is_some()
    }

    /// Check if the highlighter has a parsed tree
    pub fn has_tree(&self) -> bool {
        self.tree.is_some()
    }

    /// Compute highlights from the current syntax tree
    /// Run the highlight query over the parsed tree.
    ///
    /// Takes a `Cow<str>` from the rope rather than an owned `String`: for a
    /// single-chunk document this borrows, and only a rope spanning several leaves
    /// pays for a join. Tree-sitter's `QueryCursor` wants contiguous bytes, so this
    /// is the one place a copy can still happen — and it is bounded by the query,
    /// not by every keystroke.
    fn compute_highlights(&mut self, text: &Rope) -> Result<(), HighlightError> {
        let text: std::borrow::Cow<str> = text.slice_to_cow(0..text.len());
        let text: &str = &text;
        self.highlights.clear();

        let tree = match &self.tree {
            Some(t) => t,
            None => return Ok(()),
        };

        let query = match &self.query {
            Some(q) => q,
            None => return Ok(()),
        };

        let mut cursor = QueryCursor::new();
        let text_bytes = text.as_bytes();

        // In tree-sitter 0.24+, we need to use a callback-based approach
        // or iterate using while let with next()
        let mut captures = cursor.captures(query, tree.root_node(), text_bytes);

        while let Some((m, capture_idx)) = captures.next() {
            let capture = &m.captures[*capture_idx];
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];

            if let Some(style) = Self::capture_to_style(capture_name) {
                let range = HighlightRange::new(node.start_byte(), node.end_byte(), style);
                self.highlights.push(range);
            }
        }

        // Sort highlights by start position for efficient range queries
        self.highlights.sort_by_key(|h| h.start);

        Ok(())
    }

    /// Convert a capture name to a highlight style
    fn capture_to_style(capture_name: &str) -> Option<HighlightStyle> {
        match capture_name {
            "keyword" | "keyword.control" | "keyword.function" | "keyword.operator"
            | "keyword.return" | "keyword.storage" => Some(HighlightStyle::Keyword),
            "string" | "string.special" => Some(HighlightStyle::String),
            "comment" | "comment.line" | "comment.block" => Some(HighlightStyle::Comment),
            "type" | "type.builtin" => Some(HighlightStyle::Type),
            "function" | "function.method" | "function.macro" => Some(HighlightStyle::Function),
            "variable" | "variable.parameter" | "variable.builtin" => {
                Some(HighlightStyle::Variable)
            }
            "constant" | "constant.builtin" | "boolean" => Some(HighlightStyle::Constant),
            "number" | "float" => Some(HighlightStyle::Number),
            "operator" => Some(HighlightStyle::Operator),
            "punctuation"
            | "punctuation.bracket"
            | "punctuation.delimiter"
            | "punctuation.special" => Some(HighlightStyle::Punctuation),
            "attribute" => Some(HighlightStyle::Attribute),
            "namespace" | "module" => Some(HighlightStyle::Namespace),
            "identifier" => Some(HighlightStyle::Identifier),
            _ => None,
        }
    }
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// Tree-sitter highlight query for Rust
/// Note: This query is compatible with tree-sitter-rust 0.23.x
/// Keywords are matched via their containing node types rather than literal strings
const RUST_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string_literal) @string
(raw_string_literal) @string
(char_literal) @string
(boolean_literal) @constant
(integer_literal) @number
(float_literal) @number

; Comments
(line_comment) @comment
(block_comment) @comment

; Types
(type_identifier) @type
(primitive_type) @type.builtin

; Functions
(function_item name: (identifier) @function)
(call_expression function: (identifier) @function)
(call_expression function: (field_expression field: (field_identifier) @function.method))
(macro_invocation macro: (identifier) @function.macro)

; Variables and parameters
(parameter pattern: (identifier) @variable.parameter)
(let_declaration pattern: (identifier) @variable)

; Struct and enum definitions
(struct_item name: (type_identifier) @type)
(enum_item name: (type_identifier) @type)
(impl_item type: (type_identifier) @type)
(trait_item name: (type_identifier) @type)

; Attributes
(attribute_item) @attribute

; Modules/namespaces
(mod_item name: (identifier) @namespace)
(use_declaration argument: (scoped_identifier path: (identifier) @namespace))

; Match keywords by their node structure
(function_item) @keyword
(let_declaration) @keyword
(if_expression) @keyword
(else_clause) @keyword
(match_expression) @keyword
(for_expression) @keyword
(while_expression) @keyword
(loop_expression) @keyword
(return_expression) @keyword
(struct_item) @keyword
(enum_item) @keyword
(impl_item) @keyword
(trait_item) @keyword
(mod_item) @keyword
(use_declaration) @keyword
(const_item) @keyword
(static_item) @keyword
(type_item) @keyword

; Fallback for identifiers
(identifier) @variable
"#;

/// Tree-sitter highlight query for Python
const PYTHON_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string) @string
(integer) @number
(float) @number
(true) @constant
(false) @constant
(none) @constant

; Comments
(comment) @comment

; Keywords
"def" @keyword
"class" @keyword
"if" @keyword
"elif" @keyword
"else" @keyword
"for" @keyword
"while" @keyword
"return" @keyword
"import" @keyword
"from" @keyword
"as" @keyword
"try" @keyword
"except" @keyword
"finally" @keyword
"with" @keyword
"pass" @keyword
"break" @keyword
"continue" @keyword
"raise" @keyword
"lambda" @keyword
"yield" @keyword
"global" @keyword
"nonlocal" @keyword
"assert" @keyword
"and" @keyword
"or" @keyword
"not" @keyword
"in" @keyword
"is" @keyword

; Functions
(function_definition name: (identifier) @function)
(call function: (identifier) @function)
(call function: (attribute attribute: (identifier) @function.method))

; Classes
(class_definition name: (identifier) @type)

; Variables and parameters
(parameters (identifier) @variable.parameter)
(assignment left: (identifier) @variable)

; Decorators
(decorator) @attribute

; Types in annotations
(type) @type
"#;

/// Tree-sitter highlight query for JavaScript
const JAVASCRIPT_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string) @string
(template_string) @string
(number) @number
(true) @constant
(false) @constant
(null) @constant
(undefined) @constant

; Comments
(comment) @comment

; Keywords
"function" @keyword
"const" @keyword
"let" @keyword
"var" @keyword
"if" @keyword
"else" @keyword
"for" @keyword
"while" @keyword
"do" @keyword
"return" @keyword
"switch" @keyword
"case" @keyword
"default" @keyword
"break" @keyword
"continue" @keyword
"throw" @keyword
"try" @keyword
"catch" @keyword
"finally" @keyword
"class" @keyword
"extends" @keyword
"new" @keyword
(this) @keyword
(super) @keyword
"import" @keyword
"export" @keyword
"from" @keyword
"async" @keyword
"await" @keyword
"yield" @keyword
"typeof" @keyword
"instanceof" @keyword
"delete" @keyword
"in" @keyword
"of" @keyword

; Functions
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function.method)
(call_expression function: (identifier) @function)
(call_expression function: (member_expression property: (property_identifier) @function.method))
(arrow_function) @function

; Classes
(class_declaration name: (identifier) @type)

; Variables
(variable_declarator name: (identifier) @variable)
(formal_parameters (identifier) @variable.parameter)

; Properties
(property_identifier) @variable
"#;

/// Tree-sitter highlight query for TypeScript (inherits from JavaScript)
const TYPESCRIPT_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string) @string
(template_string) @string
(number) @number
(true) @constant
(false) @constant
(null) @constant
(undefined) @constant

; Comments
(comment) @comment

; Keywords
"function" @keyword
"const" @keyword
"let" @keyword
"var" @keyword
"if" @keyword
"else" @keyword
"for" @keyword
"while" @keyword
"do" @keyword
"return" @keyword
"switch" @keyword
"case" @keyword
"default" @keyword
"break" @keyword
"continue" @keyword
"throw" @keyword
"try" @keyword
"catch" @keyword
"finally" @keyword
"class" @keyword
"extends" @keyword
"implements" @keyword
"new" @keyword
(this) @keyword
(super) @keyword
"import" @keyword
"export" @keyword
"from" @keyword
"async" @keyword
"await" @keyword
"yield" @keyword
"typeof" @keyword
"instanceof" @keyword
"delete" @keyword
"in" @keyword
"of" @keyword
"type" @keyword
"interface" @keyword
"enum" @keyword
"namespace" @keyword
"abstract" @keyword
"private" @keyword
"protected" @keyword
"public" @keyword
"readonly" @keyword
"static" @keyword

; Types
(type_identifier) @type
(predefined_type) @type.builtin

; Functions
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function.method)
(call_expression function: (identifier) @function)
(call_expression function: (member_expression property: (property_identifier) @function.method))
(arrow_function) @function

; Classes and interfaces
(class_declaration name: (type_identifier) @type)
(interface_declaration name: (type_identifier) @type)

; Variables
(variable_declarator name: (identifier) @variable)
(required_parameter (identifier) @variable.parameter)
(optional_parameter (identifier) @variable.parameter)
"#;

/// Tree-sitter highlight query for TSX
const TSX_HIGHLIGHTS_QUERY: &str = TYPESCRIPT_HIGHLIGHTS_QUERY;

/// Tree-sitter highlight query for Go
const GO_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(interpreted_string_literal) @string
(raw_string_literal) @string
(rune_literal) @string
(int_literal) @number
(float_literal) @number
(imaginary_literal) @number
(true) @constant
(false) @constant
(nil) @constant

; Comments
(comment) @comment

; Keywords
"func" @keyword
"package" @keyword
"import" @keyword
"var" @keyword
"const" @keyword
"type" @keyword
"struct" @keyword
"interface" @keyword
"if" @keyword
"else" @keyword
"for" @keyword
"range" @keyword
"switch" @keyword
"case" @keyword
"default" @keyword
"return" @keyword
"break" @keyword
"continue" @keyword
"goto" @keyword
"fallthrough" @keyword
"defer" @keyword
"go" @keyword
"select" @keyword
"chan" @keyword
"map" @keyword

; Functions
(function_declaration name: (identifier) @function)
(method_declaration name: (field_identifier) @function.method)
(call_expression function: (identifier) @function)
(call_expression function: (selector_expression field: (field_identifier) @function.method))

; Types
(type_identifier) @type
(type_spec name: (type_identifier) @type)

; Variables
(parameter_declaration (identifier) @variable.parameter)
(short_var_declaration left: (expression_list (identifier) @variable))
(var_declaration (var_spec name: (identifier) @variable))

; Package names
(package_identifier) @namespace
"#;

/// Tree-sitter highlight query for C
const C_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string_literal) @string
(char_literal) @string
(number_literal) @number
(true) @constant
(false) @constant
(null) @constant

; Comments
(comment) @comment

; Keywords
"if" @keyword
"else" @keyword
"for" @keyword
"while" @keyword
"do" @keyword
"switch" @keyword
"case" @keyword
"default" @keyword
"break" @keyword
"continue" @keyword
"return" @keyword
"goto" @keyword
"typedef" @keyword
"struct" @keyword
"union" @keyword
"enum" @keyword
"sizeof" @keyword
"static" @keyword
"extern" @keyword
"const" @keyword
"volatile" @keyword
"inline" @keyword
"register" @keyword
"restrict" @keyword

; Types
(type_identifier) @type
(primitive_type) @type.builtin

; Functions
(function_declarator declarator: (identifier) @function)
(call_expression function: (identifier) @function)

; Variables
(parameter_declaration declarator: (identifier) @variable.parameter)
(declaration declarator: (init_declarator declarator: (identifier) @variable))

; Preprocessor
(preproc_include) @attribute
(preproc_def) @attribute
(preproc_ifdef) @attribute
(preproc_directive) @attribute
"#;

/// Tree-sitter highlight query for C++
const CPP_HIGHLIGHTS_QUERY: &str = r#"
; Literals
(string_literal) @string
(raw_string_literal) @string
(char_literal) @string
(number_literal) @number
(true) @constant
(false) @constant
(null) @constant

; Comments
(comment) @comment

; Keywords
"if" @keyword
"else" @keyword
"for" @keyword
"while" @keyword
"do" @keyword
"switch" @keyword
"case" @keyword
"default" @keyword
"break" @keyword
"continue" @keyword
"return" @keyword
"goto" @keyword
"typedef" @keyword
"struct" @keyword
"union" @keyword
"enum" @keyword
"class" @keyword
"public" @keyword
"private" @keyword
"protected" @keyword
"virtual" @keyword
"override" @keyword
"final" @keyword
"friend" @keyword
"sizeof" @keyword
"static" @keyword
"extern" @keyword
"const" @keyword
"constexpr" @keyword
"volatile" @keyword
"inline" @keyword
"register" @keyword
"mutable" @keyword
"explicit" @keyword
"namespace" @keyword
"using" @keyword
"template" @keyword
"typename" @keyword
"new" @keyword
"delete" @keyword
"try" @keyword
"catch" @keyword
"throw" @keyword
"noexcept" @keyword
(auto) @keyword
"decltype" @keyword

; Types
(type_identifier) @type
(primitive_type) @type.builtin
(class_specifier name: (type_identifier) @type)
(struct_specifier name: (type_identifier) @type)

; Functions
(function_declarator declarator: (identifier) @function)
(call_expression function: (identifier) @function)
(call_expression function: (field_expression field: (field_identifier) @function.method))

; Variables
(parameter_declaration declarator: (identifier) @variable.parameter)
(declaration declarator: (init_declarator declarator: (identifier) @variable))

; Preprocessor
(preproc_include) @attribute
(preproc_def) @attribute
(preproc_ifdef) @attribute
(preproc_directive) @attribute

; Namespace
(namespace_identifier) @namespace
"#;

/// Tree-sitter highlight query for JSON
const JSON_HIGHLIGHTS_QUERY: &str = r#"
(string) @string
(number) @number
(true) @constant
(false) @constant
(null) @constant
(pair key: (string) @variable)
"#;

/// Tree-sitter highlight query for HTML
const HTML_HIGHLIGHTS_QUERY: &str = r#"
(tag_name) @keyword
(attribute_name) @variable
(attribute_value) @string
(quoted_attribute_value) @string
(text) @string
(comment) @comment
(doctype) @attribute
"#;

/// Tree-sitter highlight query for CSS
const CSS_HIGHLIGHTS_QUERY: &str = r#"
(tag_name) @type
(class_name) @variable
(id_name) @constant
(property_name) @variable
(string_value) @string
(integer_value) @number
(float_value) @number
(color_value) @constant
(plain_value) @string
(comment) @comment
(pseudo_class_selector (class_name) @function)
(pseudo_element_selector (tag_name) @function)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // **Feature: forge-foundation, Property 11: Syntax Highlighting Validity**
    // *For any* valid Rust source code, parsing with Tree-sitter SHALL produce highlight ranges
    // where each range has valid start/end positions within the buffer bounds, and different
    // token types (keywords, strings, comments, types, identifiers) SHALL receive distinct
    // highlight styles.
    // **Validates: Requirements 5.1, 5.3**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_syntax_highlighting_validity(
            // Generate valid Rust code snippets
            code in rust_code_strategy(),
        ) {
            let rope = Rope::from(&code);
            let mut highlighter = SyntaxHighlighter::new();

            // Set language to Rust
            highlighter.set_language_enum(SupportedLanguage::Rust).unwrap();

            // Parse the code
            highlighter.parse(&rope).unwrap();

            let text_len = code.len();
            let highlights = highlighter.highlights();

            // Property 1: All highlight ranges must have valid positions within buffer bounds
            for range in highlights {
                prop_assert!(
                    range.start <= text_len,
                    "Highlight start {} exceeds buffer length {}",
                    range.start,
                    text_len
                );
                prop_assert!(
                    range.end <= text_len,
                    "Highlight end {} exceeds buffer length {}",
                    range.end,
                    text_len
                );
                prop_assert!(
                    range.start <= range.end,
                    "Highlight start {} is after end {}",
                    range.start,
                    range.end
                );
            }

            // Property 2: Keywords should be highlighted as keywords
            if code.contains("fn ") || code.contains("let ") || code.contains("if ")
               || code.contains("struct ") || code.contains("impl ") {
                let has_keyword_highlight = highlights.iter().any(|h| h.style == HighlightStyle::Keyword);
                prop_assert!(
                    has_keyword_highlight,
                    "Code with keywords should have keyword highlights: {}",
                    code
                );
            }

            // Property 3: String literals should be highlighted as strings
            if code.contains('"') && !code.contains("//") {
                // Only check if there's a string that's not in a comment
                let string_start = code.find('"');
                let comment_start = code.find("//");
                if string_start.is_some() && (comment_start.is_none() || string_start < comment_start) {
                    let has_string_highlight = highlights.iter().any(|h| h.style == HighlightStyle::String);
                    prop_assert!(
                        has_string_highlight,
                        "Code with string literals should have string highlights: {}",
                        code
                    );
                }
            }

            // Property 4: Comments should be highlighted as comments
            if code.contains("//") || code.contains("/*") {
                let has_comment_highlight = highlights.iter().any(|h| h.style == HighlightStyle::Comment);
                prop_assert!(
                    has_comment_highlight,
                    "Code with comments should have comment highlights: {}",
                    code
                );
            }

            // Property 5: Numbers should be highlighted as numbers
            if code.chars().any(|c| c.is_ascii_digit()) {
                // Check if there's a standalone number (not part of identifier)
                let has_standalone_number = code.split_whitespace()
                    .any(|word| word.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '_'));
                if has_standalone_number {
                    let has_number_highlight = highlights.iter().any(|h| h.style == HighlightStyle::Number);
                    prop_assert!(
                        has_number_highlight,
                        "Code with number literals should have number highlights: {}",
                        code
                    );
                }
            }
        }


        #[test]
        fn prop_incremental_update_produces_valid_highlights(
            initial_code in rust_code_strategy(),
            insert_text in "[a-zA-Z0-9_ ]{0,20}",
            insert_pos_factor in 0.0f64..=1.0f64,
        ) {
            let mut rope = Rope::from(&initial_code);
            let mut highlighter = SyntaxHighlighter::new();

            highlighter.set_language_enum(SupportedLanguage::Rust).unwrap();
            highlighter.parse(&rope).unwrap();

            // Byte offsets throughout. `lapce_xi_rope` is byte-addressed and so is
            // tree-sitter's `InputEdit`, so the char-offset arithmetic this test
            // used to do was a conversion in both directions for nothing — and it
            // was only there because the highlighter used to hold a second rope.
            let len = rope.len();
            let raw = ((insert_pos_factor * len as f64) as usize).min(len);
            // Never split a character: an offset mid-UTF-8 panics the rope.
            let insert_pos = (0..=raw).rev()
                .find(|&i| initial_code.is_char_boundary(i))
                .unwrap_or(0);

            let start_byte = insert_pos;
            let old_end_byte = insert_pos;
            let new_end_byte = insert_pos + insert_text.len();

            let start_line = rope.line_of_offset(insert_pos);
            let start_col = insert_pos - rope.offset_of_line(start_line);

            rope.edit(insert_pos..insert_pos, insert_text.as_str());

            let new_end_line = rope.line_of_offset(new_end_byte.min(rope.len()));
            let new_end_col = new_end_byte.saturating_sub(rope.offset_of_line(new_end_line));

            // Update the highlighter
            let result = highlighter.update(
                &rope,
                start_byte,
                old_end_byte,
                new_end_byte,
                (start_line, start_col),
                (start_line, start_col),
                (new_end_line, new_end_col),
            );

            prop_assert!(result.is_ok(), "Update should succeed");

            // Verify all highlights are still valid
            let text_len = rope.len();

            for range in highlighter.highlights() {
                prop_assert!(
                    range.start <= text_len,
                    "After update, highlight start {} exceeds buffer length {}",
                    range.start,
                    text_len
                );
                prop_assert!(
                    range.end <= text_len,
                    "After update, highlight end {} exceeds buffer length {}",
                    range.end,
                    text_len
                );
            }
        }
    }

    /// Strategy for generating valid Rust code snippets
    fn rust_code_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            // Simple function
            Just("fn main() {}".to_string()),
            Just("fn foo() { let x = 42; }".to_string()),
            Just("fn bar(x: i32) -> i32 { x + 1 }".to_string()),
            // With strings
            Just(r#"fn hello() { let s = "world"; }"#.to_string()),
            Just(r#"let msg = "Hello, World!";"#.to_string()),
            // With comments
            Just("// This is a comment\nfn main() {}".to_string()),
            Just("/* block comment */ fn test() {}".to_string()),
            // With types
            Just("struct Point { x: i32, y: i32 }".to_string()),
            Just("impl Point { fn new() -> Self { Self { x: 0, y: 0 } } }".to_string()),
            // With numbers
            Just("let x = 42;".to_string()),
            Just("let pi = 3.14;".to_string()),
            Just("let hex = 0xFF;".to_string()),
            // With control flow
            Just("if true { 1 } else { 0 }".to_string()),
            Just("for i in 0..10 { println!(\"{}\", i); }".to_string()),
            Just("while x > 0 { x -= 1; }".to_string()),
            Just("match x { 1 => \"one\", _ => \"other\" }".to_string()),
            // Empty and minimal
            Just("".to_string()),
            Just("fn f() {}".to_string()),
            // Complex
            Just("pub struct Foo<T> { data: Vec<T> }".to_string()),
            Just("use std::collections::HashMap;".to_string()),
            Just("#[derive(Debug)]\nstruct Test;".to_string()),
        ]
    }

    /// Every language's highlight query must compile against its own grammar.
    ///
    /// The queries in this file are hand-written and reference grammar node
    /// names (`string_literal`, `function_item`, …) directly. A grammar upgrade
    /// that renames or removes a node does not fail the build — `Query::new`
    /// fails at *runtime*, and only for the language whose file you happen to
    /// open. Before this test only Rust was covered, so a tree-sitter bump could
    /// silently break highlighting for the other ten and every test still
    /// passed. Asserting the requirement — "every supported language can
    /// actually be highlighted" — is what makes a future bump safe to do.
    #[test]
    fn every_supported_language_actually_highlights_something() {
        // A comment and a string in each language — the two captures every one
        // of these queries defines. Asserting on *output* rather than on the
        // query merely compiling: a query can load fine and still match
        // nothing, which looks identical to broken highlighting on screen.
        let sample = |lang: SupportedLanguage| -> &'static str {
            match lang {
                SupportedLanguage::Rust => "// c\nfn f() { let s = \"str\"; }",
                SupportedLanguage::Python => "# c\ndef f():\n    s = \"str\"",
                SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => {
                    "// c\nfunction f() { const s = \"str\"; }"
                }
                SupportedLanguage::Tsx => "// c\nconst f = () => { const s = \"str\"; };",
                SupportedLanguage::Go => "// c\nfunc f() { s := \"str\" }",
                SupportedLanguage::C => "// c\nint f() { char *s = \"str\"; }",
                SupportedLanguage::Cpp => "// c\nint f() { auto s = \"str\"; }",
                SupportedLanguage::Json => "{\"k\": \"str\"}",
                SupportedLanguage::Html => "<!-- c --><p class=\"x\">t</p>",
                SupportedLanguage::Css => "/* c */\na { content: \"str\"; }",
            }
        };

        let mut broken = Vec::new();
        for lang in SupportedLanguage::all() {
            let mut highlighter = SyntaxHighlighter::new();
            if let Err(e) = highlighter.set_language_enum(*lang) {
                broken.push(format!("{}: query failed to compile: {e}", lang.id()));
                continue;
            }
            let rope = Rope::from(sample(*lang));
            if let Err(e) = highlighter.parse(&rope) {
                broken.push(format!("{}: parse failed: {e}", lang.id()));
                continue;
            }
            if highlighter.highlights().is_empty() {
                broken.push(format!(
                    "{}: query loaded but produced no highlights",
                    lang.id()
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "syntax highlighting is broken for:\n  {}",
            broken.join("\n  ")
        );
    }

    #[test]
    fn parsing_rust_produces_keyword_and_number_highlights() {
        let mut highlighter = SyntaxHighlighter::new();
        highlighter
            .set_language_enum(SupportedLanguage::Rust)
            .unwrap();

        let code = "fn main() { let x = 42; }";
        let rope = Rope::from(code);

        highlighter.parse(&rope).unwrap();

        assert!(highlighter.has_tree());
        assert!(!highlighter.highlights().is_empty());

        // Should have keyword highlights for 'fn' and 'let'
        let keywords: Vec<_> = highlighter
            .highlights()
            .iter()
            .filter(|h| h.style == HighlightStyle::Keyword)
            .collect();
        assert!(!keywords.is_empty(), "Should have keyword highlights");

        // Should have number highlight for '42'
        let numbers: Vec<_> = highlighter
            .highlights()
            .iter()
            .filter(|h| h.style == HighlightStyle::Number)
            .collect();
        assert!(!numbers.is_empty(), "Should have number highlights");
    }

    #[test]
    fn different_token_kinds_get_different_styles() {
        let mut highlighter = SyntaxHighlighter::new();
        highlighter
            .set_language_enum(SupportedLanguage::Rust)
            .unwrap();

        let code = r#"
            // comment
            fn main() {
                let s = "string";
                let n = 42;
            }
        "#;
        let rope = Rope::from(code);

        highlighter.parse(&rope).unwrap();

        let highlights = highlighter.highlights();

        // Collect unique styles
        let mut styles: Vec<HighlightStyle> = highlights.iter().map(|h| h.style).collect();
        styles.sort_by_key(|s| format!("{:?}", s));
        styles.dedup();

        // Should have multiple distinct styles
        assert!(
            styles.len() >= 3,
            "Should have at least 3 distinct styles, got: {:?}",
            styles
        );

        // Should include keyword, string, comment, and number
        assert!(
            styles.contains(&HighlightStyle::Keyword),
            "Should have keyword style"
        );
        assert!(
            styles.contains(&HighlightStyle::String),
            "Should have string style"
        );
        assert!(
            styles.contains(&HighlightStyle::Comment),
            "Should have comment style"
        );
        assert!(
            styles.contains(&HighlightStyle::Number),
            "Should have number style"
        );
    }
}
