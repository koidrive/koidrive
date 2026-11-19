// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Read};

use proc_macro2::Span;
use quote::ToTokens;
use serde_json::{Value, json};
use syn::spanned::Spanned;

struct Offsets<'a> {
    source: &'a str,
    lines: Vec<usize>,
}

impl Offsets<'_> {
    fn at(&self, location: proc_macro2::LineColumn) -> usize {
        let line = self.lines[location.line - 1];
        line + self.source[line..]
            .chars()
            .take(location.column)
            .map(char::len_utf8)
            .sum::<usize>()
    }

    fn span(&self, span: Span) -> (usize, usize) {
        (self.at(span.start()), self.at(span.end()))
    }
}

fn include_path(m: &syn::ItemMacro) -> Option<String> {
    if !m.mac.path.is_ident("include") {
        return None;
    }
    let path: syn::LitStr =
        syn::parse2(m.mac.tokens.clone()).expect("include path must be a string literal");
    Some(path.value())
}

fn proptest_name(m: &syn::ItemMacro) -> Option<String> {
    if m.mac.path.segments.last().unwrap().ident != "proptest" {
        return None;
    }
    let tokens: Vec<_> = m.mac.tokens.clone().into_iter().collect();
    let names: Vec<_> = tokens
        .windows(2)
        .filter(|pair| pair[0].to_string() == "fn")
        .map(|pair| pair[1].to_string())
        .collect();
    assert!(!names.is_empty(), "proptest must contain named functions");
    Some(format!("proptest:{}", names.join(",")))
}

fn flat(file: syn::File, offsets: &Offsets, fragment: bool) -> Vec<Value> {
    let mut items = Vec::new();
    for item in file.items {
        let (name, kind) = match &item {
            syn::Item::Fn(f) => (
                format!("fn:{}", f.sig.ident),
                if f.attrs.iter().any(|a| a.path().is_ident("test")) {
                    "test"
                } else {
                    "helper"
                },
            ),
            syn::Item::Const(c) => (format!("const:{}", c.ident), "helper"),
            syn::Item::Static(s) => (format!("static:{}", s.ident), "helper"),
            syn::Item::Struct(s) => (format!("struct:{}", s.ident), "helper"),
            syn::Item::Enum(e) => (format!("enum:{}", e.ident), "helper"),
            syn::Item::Type(t) => (format!("type:{}", t.ident), "helper"),
            syn::Item::Mod(m) => (format!("mod:{}", m.ident), "module"),
            syn::Item::Use(u) => (format!("use:{}", u.tree.to_token_stream()), "helper"),
            syn::Item::Impl(i) => (impl_name(i), "helper"),
            syn::Item::Macro(m) if proptest_name(m).is_some() => {
                (proptest_name(m).unwrap(), "property")
            }
            syn::Item::Macro(m) if m.mac.path.is_ident("include") => {
                assert!(!fragment, "scenario fragments cannot include other files");
                assert!(
                    m.attrs.is_empty(),
                    "include statements must be unconditional and unadorned"
                );
                (format!("include:{}", include_path(m).unwrap()), "include")
            }
            other => panic!("unsupported item: {}", other.to_token_stream()),
        };
        let (start, end) = offsets.span(item.span());
        items.push(json!({"name": name, "kind": kind, "start": start, "end": end}));
    }
    items
}

fn impl_name(i: &syn::ItemImpl) -> String {
    format!(
        "impl:{}:{}",
        i.trait_
            .as_ref()
            .map(|(_, t, _)| t.to_token_stream().to_string())
            .unwrap_or_default(),
        i.self_ty.to_token_stream()
    )
}

fn conditions(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg"))
        .map(|a| format!("[{}]", a.meta.to_token_stream()))
        .collect()
}

fn path_attribute(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| match &a.meta {
        syn::Meta::NameValue(value) if value.path.is_ident("path") => match &value.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(path),
                ..
            }) => Some(path.value()),
            _ => None,
        },
        _ => None,
    })
}

fn entry(parent: &[String], name: String, kind: &str, (start, end): (usize, usize)) -> Value {
    json!({"parent": parent, "name": name, "kind": kind, "start": start, "end": end})
}

fn literals(tokens: proc_macro2::TokenStream, offsets: &Offsets, out: &mut Vec<(usize, usize)>) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Group(group) => literals(group.stream(), offsets, out),
            proc_macro2::TokenTree::Literal(literal)
                if literal.span().start().line != literal.span().end().line =>
            {
                out.push(offsets.span(literal.span()));
            }
            _ => {}
        }
    }
}

fn spanned(mut found: Value, tokens: proc_macro2::TokenStream, offsets: &Offsets) -> Value {
    let mut spans = Vec::new();
    literals(tokens, offsets, &mut spans);
    found["literals"] = json!(spans);
    found
}

fn nested_item(item: &syn::Item, parent: &[String], offsets: &Offsets, out: &mut Vec<Value>) {
    let span = offsets.span(item.span());
    let named = |name: String, attrs: &[syn::Attribute]| format!("{name}{}", conditions(attrs));
    let (name, kind) = match item {
        syn::Item::Fn(f) => (named(format!("fn:{}", f.sig.ident), &f.attrs), "item"),
        syn::Item::Const(c) => (named(format!("const:{}", c.ident), &c.attrs), "item"),
        syn::Item::Static(s) => (named(format!("static:{}", s.ident), &s.attrs), "item"),
        syn::Item::Struct(s) => (named(format!("struct:{}", s.ident), &s.attrs), "item"),
        syn::Item::Enum(e) => (named(format!("enum:{}", e.ident), &e.attrs), "item"),
        syn::Item::Type(t) => (named(format!("type:{}", t.ident), &t.attrs), "item"),
        syn::Item::Trait(t) => (named(format!("trait:{}", t.ident), &t.attrs), "item"),
        syn::Item::Union(u) => (named(format!("union:{}", u.ident), &u.attrs), "item"),
        syn::Item::Use(u) => (
            named(format!("use:{}", u.tree.to_token_stream()), &u.attrs),
            "use",
        ),
        syn::Item::Macro(m) if include_path(m).is_some() => {
            assert!(
                m.attrs.is_empty(),
                "include statements must be unconditional and unadorned"
            );
            (include_path(m).unwrap(), "include")
        }
        syn::Item::Macro(m) => (
            named(
                proptest_name(m).unwrap_or_else(|| {
                    format!(
                        "macro:{}:{}",
                        m.mac.path.to_token_stream(),
                        m.ident
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    )
                }),
                &m.attrs,
            ),
            "item",
        ),
        syn::Item::Mod(m) => {
            let name = named(format!("mod:{}", m.ident), &m.attrs);
            let Some((_, items)) = &m.content else {
                let mut declared = entry(parent, name, "module", span);
                declared["file"] = json!(m.ident.to_string());
                declared["path"] = json!(path_attribute(&m.attrs));
                out.push(declared);
                return;
            };
            out.push(entry(parent, name.clone(), "container", span));
            let inner = [parent, &[name]].concat();
            for item in items {
                nested_item(item, &inner, offsets, out);
            }
            return;
        }
        syn::Item::Impl(i) => {
            let name = named(impl_name(i), &i.attrs);
            out.push(entry(parent, name.clone(), "container", span));
            let inner = [parent, &[name]].concat();
            for member in &i.items {
                let name = match member {
                    syn::ImplItem::Fn(f) => named(format!("fn:{}", f.sig.ident), &f.attrs),
                    syn::ImplItem::Const(c) => named(format!("const:{}", c.ident), &c.attrs),
                    syn::ImplItem::Type(t) => named(format!("type:{}", t.ident), &t.attrs),
                    syn::ImplItem::Macro(m) => {
                        named(format!("macro:{}", m.mac.path.to_token_stream()), &m.attrs)
                    }
                    other => format!("member:{}", other.to_token_stream()),
                };
                let found = entry(&inner, name, "item", offsets.span(member.span()));
                out.push(spanned(found, member.to_token_stream(), offsets));
            }
            return;
        }
        other => (format!("item:{}", other.to_token_stream()), "item"),
    };
    out.push(spanned(
        entry(parent, name, kind, span),
        item.to_token_stream(),
        offsets,
    ));
}

fn main() {
    let mut source = String::new();
    io::stdin().read_to_string(&mut source).unwrap();
    let file = syn::parse_file(&source).expect("parse Rust source");
    let fragment = std::env::args().any(|arg| arg == "--fragment");
    assert!(
        !fragment || file.attrs.is_empty(),
        "scenario fragments cannot have inner attributes"
    );
    let mut lines = vec![0];
    for (offset, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            lines.push(offset + 1);
        }
    }
    let offsets = Offsets {
        source: &source,
        lines,
    };
    let items = if std::env::args().any(|arg| arg == "--nested") {
        let mut out = Vec::new();
        for item in &file.items {
            nested_item(item, &[], &offsets, &mut out);
        }
        out
    } else {
        flat(file, &offsets, fragment)
    };
    println!("{}", json!(items));
}
