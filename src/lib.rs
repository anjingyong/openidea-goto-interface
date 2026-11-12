//! Go to Interface: jump from a Go method to the interface method it satisfies.
//!
//! The IDE has no Go language server, so everything here is inferred from the
//! text. Two routes are tried, cheapest first:
//!
//! 1. `var _ pkg.Iface = &T{}` assertions in the package. They name the
//!    interface explicitly, so it is resolved through the IDE's symbol index —
//!    the only way to reach interfaces in other packages (generated gRPC
//!    stubs, most often).
//! 2. Otherwise, `type X interface { ... }` blocks of the *same* package.
//!
//! Anything ambiguous stays silent — a marker that jumps to the wrong place is
//! worse than no marker.

use std::collections::HashMap;

use openidea_plugin::host;
use openidea_plugin::providers::{Document, LineMarker, Providers};
use openidea_plugin::{
    Error, Invocation, Location, NotificationLevel, Plugin, Position, Project, Range,
    register_plugin,
};

/// Upper bound on package files scanned per request (editing path budget).
const MAX_FILES: u32 = 200;
/// Upper bound on symbol-index probes per request.
const MAX_SYMBOLS: u32 = 20;

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
    /// Receiver base type (`service` for `func (s *service)`).
    recv: String,
    name: String,
    params: usize,
    /// 0-based line of the `func` line.
    line: u32,
}

/// `var _ pkg.Iface = &T{}` — "T is meant to satisfy Iface".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Assertion {
    recv: String,
    iface: String,
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

/// Base type name of a type expression: `*service`, `service[T]`,
/// `(*baz)(nil)` -> `service`, `service`, `baz`.
fn base_type(expr: &str) -> String {
    let mut s = expr.trim();
    s = s.trim_start_matches('(');
    s = s.trim_start_matches('&');
    s = s.trim_start_matches('*');
    s = s.trim_end_matches(')');
    for stop in ['[', '{', ' ', ')', '*'] {
        if let Some(idx) = s.find(stop) {
            s = &s[..idx];
        }
    }
    s.trim().to_string()
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

/// `func (c *T) Name(a int) err` -> Some(("T", "Name", 1)); top-level functions
/// (no receiver) return None — they cannot satisfy an interface.
fn method_signature(line: &str) -> Option<(String, String, usize)> {
    let rest = line.strip_prefix("func")?;
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = matching_paren(rest, 0)?;
    let recv = recv_type(&rest[1..close])?;
    let after = rest[close + 1..].trim_start();
    let (name, params) = iface_signature(after)?;
    Some((recv, name, params))
}

/// `s *service` -> Some("service").
fn recv_type(recv: &str) -> Option<String> {
    let last = recv.split_whitespace().last()?;
    let ty = base_type(last);
    if ty.is_empty() || !ty.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
        return None;
    }
    Some(ty)
}

/// `type X interface {` -> Some("X").
fn interface_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("type")?;
    let rest = rest.trim_start();
    if rest.starts_with('(') {
        return None; // grouped type declaration: not supported yet
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
        if let Some((recv, name, params)) = method_signature(code) {
            out.push(GoMethod {
                recv,
                name,
                params,
                line: idx as u32,
            });
        }
    }
    out
}

/// `var _ pkg.Iface = &T{}` / `= (*T)(nil)` declarations in this file.
fn parse_assertions(text: &str) -> Vec<Assertion> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let code = strip_comment(raw).trim();
        let Some(rest) = code.strip_prefix("var _") else {
            continue;
        };
        let Some((left, right)) = rest.split_once('=') else {
            continue;
        };
        let iface = left.trim().rsplit('.').next().unwrap_or("").trim();
        if iface.is_empty() {
            continue;
        }
        let rhs = right.trim();
        if !(rhs.starts_with('&') || rhs.starts_with("(*")) {
            continue;
        }
        let recv = base_type(rhs);
        if recv.is_empty() {
            continue;
        }
        out.push(Assertion {
            recv,
            iface: iface.to_string(),
        });
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
    /// package dir -> (document version, same-package interfaces, assertions)
    package_cache: HashMap<String, (u64, Vec<IfaceMethod>, Vec<Assertion>)>,
    /// interface name -> (document version, its methods)
    iface_cache: HashMap<String, (u64, Vec<IfaceMethod>)>,
}

impl Main {
    /// Interfaces declared in the document's own package, plus the
    /// `var _ ... = &T{}` assertions found there.
    fn scan_package(
        &mut self,
        project: &Project,
        root: &str,
        dir: &str,
        doc: &Document,
    ) -> (Vec<IfaceMethod>, Vec<Assertion>) {
        if let Some((version, ifaces, asserts)) = self.package_cache.get(dir) {
            if *version == doc.version {
                return (ifaces.clone(), asserts.clone());
            }
        }
        let rel = dir.strip_prefix(root).unwrap_or("").trim_start_matches('/');
        let glob = if rel.is_empty() {
            "*.go".to_string()
        } else {
            format!("{rel}/*.go")
        };
        let mut ifaces = Vec::new();
        let mut asserts = Vec::new();
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
                    ifaces.extend(parse_ifaces(&abs, &text));
                    asserts.extend(parse_assertions(&text));
                }
            }
        }
        self.package_cache
            .insert(dir.to_string(), (doc.version, ifaces.clone(), asserts.clone()));
        (ifaces, asserts)
    }

    /// Methods of a named interface, wherever it lives. The symbol index is the
    /// only project-wide lookup available at plugin API 1.3 — there is no text
    /// search here — and it is what makes cross-package interfaces reachable.
    fn resolve_iface(&mut self, project: &Project, name: &str, version: u64) -> Vec<IfaceMethod> {
        if let Some((cached_version, methods)) = self.iface_cache.get(name) {
            if *cached_version == version {
                return methods.clone();
            }
        }
        let mut found = Vec::new();
        if let Ok(symbols) = project.symbols(name, MAX_SYMBOLS) {
            for sym in symbols {
                if sym.name != name || !sym.location.path.ends_with(".go") {
                    continue;
                }
                let path = sym.location.path.clone();
                if let Ok(text) = project.read_file(&path) {
                    let methods: Vec<IfaceMethod> = parse_ifaces(&path, &text)
                        .into_iter()
                        .filter(|m| m.iface == name)
                        .collect();
                    if !methods.is_empty() {
                        found = methods;
                        break;
                    }
                }
            }
        }
        self.iface_cache
            .insert(name.to_string(), (version, found.clone()));
        found
    }

    fn markers_for(&mut self, doc: &Document) -> Vec<LineMarker> {
        // Guard on the path, not on `doc.language`: this IDE has no Go support
        // installed, so the language id may not be "go".
        if !doc.path.ends_with(".go") {
            return Vec::new();
        }
        let Some(project) = host::project_of(&doc.path) else {
            return Vec::new();
        };
        let Ok(root) = project.root() else {
            return Vec::new();
        };
        let Some(idx) = doc.path.rfind('/') else {
            return Vec::new();
        };
        let dir = doc.path[..idx].to_string();

        let (same_package, asserts) = self.scan_package(&project, &root, &dir, doc);
        let methods = parse_methods(&doc.text);

        let mut out = Vec::new();
        for method in &methods {
            // Route 1: the package asserts `var _ pkg.Iface = &T{}`.
            let asserted: Vec<&Assertion> =
                asserts.iter().filter(|a| a.recv == method.recv).collect();
            if !asserted.is_empty() {
                let mut hit: Option<IfaceMethod> = None;
                let mut ambiguous = false;
                for assertion in asserted {
                    for cand in self.resolve_iface(&project, &assertion.iface, doc.version) {
                        if cand.name == method.name && cand.params == method.params {
                            if hit.is_some() {
                                ambiguous = true;
                            } else {
                                hit = Some(cand);
                            }
                        }
                    }
                }
                if ambiguous {
                    continue;
                }
                if let Some(cand) = hit {
                    out.push(marker_for(method.line, &cand));
                    continue;
                }
            }
            // Route 2: an interface of the same package.
            if let Some(cand) = unique_match(method, &same_package) {
                out.push(marker_for(method.line, cand));
            }
        }
        out
    }
}

fn marker_for(line: u32, hit: &IfaceMethod) -> LineMarker {
    LineMarker {
        line,
        icon: Some("implementing".to_string()),
        tooltip: format!("{}.{}", hit.iface, hit.name),
        target: Some(Location {
            path: hit.path.clone(),
            range: Range::new(Position::new(hit.line, 0), Position::new(hit.line, 0)),
        }),
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
                let document = editor.document()?;
                let markers = self.markers_for(&Document {
                    path: editor.path().to_string(),
                    language: "go".to_string(),
                    version: document.version,
                    text: document.text,
                });
                let body = match markers.len() {
                    0 => "No interface method found for this function".to_string(),
                    n => format!("{n} method(s) satisfy an interface method"),
                };
                host::notify(NotificationLevel::Info, "Go to Interface", &body);
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
package cubebox

// CubeboxMgrServer is the server API for CubeboxMgr service.
type CubeboxMgrServer interface {
    Create(context.Context, *RunCubeSandboxRequest) (*RunCubeSandboxResponse, error)
    List(context.Context, *ListCubeSandboxRequest) (*ListCubeSandboxResponse, error)
}
"#;

    const SERVICE: &str = r#"
package cubebox

var _ cubebox.CubeboxMgrServer = &service{}

func (s *service) Create(ctx context.Context, req *cubebox.RunCubeSandboxRequest) (*cubebox.RunCubeSandboxResponse, error) {
    return nil, nil
}

func (s *service) helperOnly(ctx context.Context) error {
    return nil
}
"#;

    #[test]
    fn counts_params_at_top_level_only() {
        assert_eq!(count_params(""), 0);
        assert_eq!(count_params("a int"), 1);
        assert_eq!(count_params("ctx context.Context, clusterID string"), 2);
        assert_eq!(count_params("context.Context, *RunCubeSandboxRequest"), 2);
        assert_eq!(count_params("fn func(a, b int), xs []int"), 2);
        assert_eq!(count_params("m map[string]int"), 1);
    }

    #[test]
    fn parses_interface_methods() {
        let ifaces = parse_ifaces("/p/cubebox_grpc.pb.go", IFACE);
        assert_eq!(ifaces.len(), 2);
        assert_eq!(ifaces[0].iface, "CubeboxMgrServer");
        assert_eq!(ifaces[0].name, "Create");
        assert_eq!(ifaces[0].params, 2);
    }

    #[test]
    fn parses_receiver_type() {
        let methods = parse_methods(SERVICE);
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0].recv, "service");
        assert_eq!(methods[0].name, "Create");
        assert_eq!(methods[0].params, 2);
    }

    #[test]
    fn parses_assertions() {
        let asserts = parse_assertions(SERVICE);
        assert_eq!(asserts.len(), 1);
        assert_eq!(asserts[0].recv, "service");
        assert_eq!(asserts[0].iface, "CubeboxMgrServer");
    }

    #[test]
    fn parses_pointer_nil_assertion() {
        let text = "var _ foo.Bar = (*baz)(nil)\n";
        let asserts = parse_assertions(text);
        assert_eq!(asserts[0].recv, "baz");
        assert_eq!(asserts[0].iface, "Bar");
    }

    #[test]
    fn cross_package_signature_matches_by_arity() {
        // Named parameters on the implementation, unnamed in the interface.
        let ifaces = parse_ifaces("/p/pb.go", IFACE);
        let methods = parse_methods(SERVICE);
        let hit = unique_match(&methods[0], &ifaces).expect("should match");
        assert_eq!(hit.iface, "CubeboxMgrServer");
        assert_eq!(hit.name, "Create");
        assert!(unique_match(&methods[1], &ifaces).is_none());
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
            recv: "T".into(),
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
