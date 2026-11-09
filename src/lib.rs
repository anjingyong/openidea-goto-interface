//! Goto Interface: jump from a Go method to the interface method it satisfies.
//!
//! The IDE has no Go language server, so everything here is inferred from the
//! text: scan the package for `type X interface { ... }` blocks and match by
//! method name + parameter count. Anything ambiguous stays silent — a marker
//! that jumps to the wrong place is worse than no marker.

use std::collections::HashMap;

use openidea_plugin::host;
use openidea_plugin::providers::{Document, LineMarker, Providers};
use openidea_plugin::{
    Error, Invocation, Location, NotificationLevel, Plugin, Position, Range, register_plugin,
};

/// Upper bound on package files scanned per request (editing path budget).
const MAX_FILES: u32 = 200;

// ---------------------------------------------------------------- 数据模型

#[derive(Clone, Debug, PartialEq, Eq)]
struct IfaceMethod {
    iface: String,
    name: String,
    params: usize,
    path: String,
    /// 0-based line of the method inside the interface block.
    line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GoMethod {
    name: String,
    params: usize,
    /// 0-based line of the `func` line.
    line: u32,
}

// ---------------------------------------------------------------- 文本工具

/// Strip a `//` line comment, respecting string literals.
fn strip_comment(line: &str) -> &str {
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match in_str {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' || c == b'`' {
                    in_str = Some(c);
                } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    return &line[..i];
                }
            }
        }
        i += 1;
    }
    line
}

/// Net brace depth change of a line, ignoring braces inside strings.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match in_str {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if c == '"' || c == '\'' || c == '`' {
                    in_str = Some(c);
                } else if c == '{' {
                    delta += 1;
                } else if c == '}' {
                    delta -= 1;
                }
            }
        }
    }
    delta
}

/// Index of the `)` matching the `(` at `open`, counting nesting.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (off, c) in s[open..].char_indices() {
        match in_str {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if c == '"' || c == '\'' || c == '`' {
                    in_str = Some(c);
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + off);
                    }
                }
            }
        }
    }
    None
}

/// Number of top-level comma-separated parameters. Type information is not
/// available here, so only the arity is compared.
fn count_params(inner: &str) -> usize {
    if inner.trim().is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    let mut has_content = false;
    for c in inner.chars() {
        match in_str {
            Some(q) => {
                has_content = true;
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if c == '"' || c == '\'' || c == '`' {
                    in_str = Some(c);
                    has_content = true;
                } else if c == '(' || c == '[' || c == '{' {
                    depth += 1;
                    has_content = true;
                } else if c == ')' || c == ']' || c == '}' {
                    depth -= 1;
                } else if c == ',' && depth == 0 {
                    if has_content {
                        count += 1;
                        has_content = false;
                    }
                } else if !c.is_whitespace() {
                    has_content = true;
                }
            }
        }
    }
    if has_content {
        count += 1;
    }
    count
}

/// `Name(a, b) result` -> ("Name", 2).
fn iface_signature(line: &str) -> Option<(String, usize)> {
    let open = line.find('(')?;
    let name = line[..open].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let close = matching_paren(line, open)?;
    Some((name.to_string(), count_params(&line[open + 1..close])))
}

/// `func (c *T) Name(a int) err` -> Some(("Name", 1)); top-level functions
/// (no receiver) return None — they cannot satisfy an interface.
fn method_signature(line: &str) -> Option<(String, usize)> {
    let rest = line.strip_prefix("func")?;
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = matching_paren(rest, 0)?;
    let after = rest[close + 1..].trim_start();
    iface_signature(after)
}

/// `type X interface {` -> Some("X").
fn interface_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("type")?;
    let rest = rest.trim_start();
    if rest.starts_with('(') {
        return None; // grouped type declaration: not supported in v0.1
    }
    let mut parts = rest.split_whitespace();
    let name = parts.next()?;
    let kw = parts.next()?;
    if !kw.starts_with("interface") || !line.contains('{') {
        return None;
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------- 解析

/// Every method declared inside an `interface` block of this file.
fn parse_ifaces(path: &str, text: &str) -> Vec<IfaceMethod> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let code = strip_comment(lines[i]).trim();
        let Some(iface) = interface_name(code) else {
            i += 1;
            continue;
        };
        // Single-line form: `type A interface{ Foo(int) error }`
        if let (Some(open), Some(close)) = (code.find('{'), code.rfind('}')) {
            if close > open {
                for seg in code[open + 1..close].split(';') {
                    if let Some((name, params)) = iface_signature(seg.trim()) {
                        out.push(IfaceMethod {
                            iface: iface.clone(),
                            name,
                            params,
                            path: path.to_string(),
                            line: i as u32,
                        });
                    }
                }
                i += 1;
                continue;
            }
        }
        // Block form.
        let mut depth = brace_delta(code);
        let mut j = i + 1;
        while j < lines.len() && depth > 0 {
            let raw = lines[j];
            let line_code = strip_comment(raw).trim();
            if !line_code.is_empty() {
                if let Some((name, params)) = iface_signature(line_code) {
                    out.push(IfaceMethod {
                        iface: iface.clone(),
                        name,
                        params,
                        path: path.to_string(),
                        line: j as u32,
                    });
                }
            }
            depth += brace_delta(raw);
            j += 1;
        }
        i = j;
    }
    out
}

/// Every receiver method declared in this file (top-level funcs excluded).
fn parse_methods(text: &str) -> Vec<GoMethod> {
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let code = strip_comment(raw).trim();
        if code.is_empty() {
            continue;
        }
        if let Some((name, params)) = method_signature(code) {
            out.push(GoMethod {
                name,
                params,
                line: idx as u32,
            });
        }
    }
    out
}

/// Pick the single interface method a Go method satisfies. `None` when there
/// are no candidates or more than one — we never guess.
fn unique_match<'a>(method: &GoMethod, ifaces: &'a [IfaceMethod]) -> Option<&'a IfaceMethod> {
    let mut hit: Option<&IfaceMethod> = None;
    for cand in ifaces {
        if cand.name == method.name && cand.params == method.params {
            if hit.is_some() {
                return None; // ambiguous
            }
            hit = Some(cand);
        }
    }
    hit
}

// ---------------------------------------------------------------- 插件

#[derive(Default)]
pub struct Main {
    /// package dir -> (document version it was built from, interface methods)
    cache: HashMap<String, (u64, Vec<IfaceMethod>)>,
}

impl Main {
    fn analyze(&mut self, doc: &Document) -> Option<(Vec<GoMethod>, Vec<IfaceMethod>)> {
        // Guard on the path, not on `doc.language`: this IDE has no Go support
        // installed, so the language id may not be "go".
        if !doc.path.ends_with(".go") {
            return None;
        }
        let project = host::project_of(&doc.path)?;
        let root = project.root().ok()?;
        let idx = doc.path.rfind('/')?;
        let dir = doc.path[..idx].to_string();

        let rel = dir
            .strip_prefix(&root)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or("")
            .to_string();
        let glob = if rel.is_empty() {
            "*.go".to_string()
        } else {
            format!("{rel}/*.go")
        };

        let ifaces = match self.cache.get(&dir) {
            Some((version, cached)) if *version == doc.version => cached.clone(),
            _ => {
                let mut found = Vec::new();
                if let Ok(files) = project.find_files(&glob, MAX_FILES) {
                    for path in files {
                        if !path.ends_with(".go") || path.ends_with("_test.go") {
                            continue;
                        }
                        let abs = if path.starts_with('/') {
                            path.clone()
                        } else {
                            format!("{root}/{}", path.trim_start_matches('/'))
                        };
                        if let Ok(text) = project.read_file(&abs) {
                            found.extend(parse_ifaces(&abs, &text));
                        }
                    }
                }
                self.cache.insert(dir.clone(), (doc.version, found.clone()));
                found
            }
        };

        Some((parse_methods(&doc.text), ifaces))
    }

    fn markers_for(&mut self, doc: &Document) -> Vec<LineMarker> {
        let Some((methods, ifaces)) = self.analyze(doc) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for method in &methods {
            let Some(hit) = unique_match(method, &ifaces) else {
                continue;
            };
            out.push(LineMarker {
                line: method.line,
                icon: Some("implementing".to_string()),
                tooltip: format!("{}.{}", hit.iface, hit.name),
                target: Some(Location {
                    path: hit.path.clone(),
                    range: Range::new(Position::new(hit.line, 0), Position::new(hit.line, 0)),
                }),
            });
        }
        out
    }
}

impl Providers for Main {
    fn line_markers(&mut self, doc: &Document) -> Vec<LineMarker> {
        self.markers_for(doc)
    }

    fn definition(&mut self, doc: &Document, at: Position) -> Vec<Location> {
        self.markers_for(doc)
            .into_iter()
            .find(|m| m.line == at.line)
            .and_then(|m| m.target)
            .into_iter()
            .collect()
    }
}

impl Plugin for Main {
    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        match id {
            "goto-interface" => {
                let editor = ctx.editor().ok_or("no file is open")?;
                let doc = editor.document()?;
                let markers = self.markers_for(&Document {
                    path: editor.path().to_string(),
                    language: "go".to_string(),
                    version: doc.version,
                    text: doc.text,
                });
                let body = match markers.len() {
                    0 => "No interface method found for this function".to_string(),
                    n => format!("{n} method(s) satisfy an interface method"),
                };
                host::notify(NotificationLevel::Info, "Goto Interface", &body);
                Ok(None)
            }
            _ => Err(format!("unknown command {id}").into()),
        }
    }
}

register_plugin!(Main, providers);

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    const IFACE: &str = r#"
package template

type TemplateService interface {
    ListTemplates(ctx context.Context, clusterID string) ([]Template, error)
    DeleteTemplate(ctx context.Context, clusterID, id string) error
}
"#;

    const IMPL: &str = r#"
package template

// ListTemplates 列出集群下的模板。
func (c *httpClient) ListTemplates(ctx context.Context, clusterID string) ([]Template, error) {
    return nil, nil
}

func (c *httpClient) DeleteTemplate(ctx context.Context, clusterID, id string) error {
    return nil
}

func helper(a, b int) int { return a + b }
"#;

    #[test]
    fn counts_params_at_top_level_only() {
        assert_eq!(count_params(""), 0);
        assert_eq!(count_params("a int"), 1);
        assert_eq!(count_params("ctx context.Context, clusterID string"), 2);
        assert_eq!(count_params("ctx context.Context, clusterID, id string"), 3);
        assert_eq!(count_params("fn func(a, b int), xs []int"), 2);
        assert_eq!(count_params("m map[string]int"), 1);
    }

    #[test]
    fn parses_interface_methods() {
        let ifaces = parse_ifaces("/p/template/iface.go", IFACE);
        assert_eq!(ifaces.len(), 2);
        assert_eq!(ifaces[0].iface, "TemplateService");
        assert_eq!(ifaces[0].name, "ListTemplates");
        assert_eq!(ifaces[0].params, 2);
        assert_eq!(ifaces[1].params, 3);
        assert_eq!(ifaces[0].line, 4);
    }

    #[test]
    fn ignores_top_level_functions() {
        let methods = parse_methods(IMPL);
        let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["ListTemplates", "DeleteTemplate"]);
        assert_eq!(methods[0].line, 4);
    }

    #[test]
    fn matches_unique_pair() {
        let ifaces = parse_ifaces("/p/template/iface.go", IFACE);
        let methods = parse_methods(IMPL);
        let hit = unique_match(&methods[0], &ifaces).expect("should match");
        assert_eq!(hit.iface, "TemplateService");
        assert_eq!(hit.line, 4);
    }

    #[test]
    fn arity_mismatch_is_not_a_match() {
        let ifaces = parse_ifaces("/p/template/iface.go", IFACE);
        let odd = GoMethod {
            name: "ListTemplates".into(),
            params: 3,
            line: 9,
        };
        assert!(unique_match(&odd, &ifaces).is_none());
    }

    #[test]
    fn duplicate_declarations_stay_silent() {
        let text = r#"
type A interface {
    Foo(x int) error
}

type B interface {
    Foo(x int) error
}
"#;
        let ifaces = parse_ifaces("/p/x.go", text);
        let method = GoMethod {
            name: "Foo".into(),
            params: 1,
            line: 0,
        };
        assert!(unique_match(&method, &ifaces).is_none());
    }

    #[test]
    fn single_line_interface_is_parsed() {
        let ifaces = parse_ifaces("/p/x.go", "type A interface{ Foo(x int) error }");
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Foo");
    }

    #[test]
    fn commented_out_code_is_ignored() {
        let text = "// func (c *T) Foo(a int) error {\nfunc (c *T) Bar() {}\n";
        let methods = parse_methods(text);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "Bar");
    }
}
