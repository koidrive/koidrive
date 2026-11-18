// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Read};

use quote::ToTokens;
use serde_json::json;
use syn::spanned::Spanned;

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
    let offset = |location: proc_macro2::LineColumn| {
        let line = lines[location.line - 1];
        line + source[line..]
            .chars()
            .take(location.column)
            .map(char::len_utf8)
            .sum::<usize>()
    };
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
            syn::Item::Impl(i) => (
                format!(
                    "impl:{}:{}",
                    i.trait_
                        .as_ref()
                        .map(|(_, t, _)| t.to_token_stream().to_string())
                        .unwrap_or_default(),
                    i.self_ty.to_token_stream()
                ),
                "helper",
            ),
            syn::Item::Macro(m) if m.mac.path.segments.last().unwrap().ident == "proptest" => {
                let tokens: Vec<_> = m.mac.tokens.clone().into_iter().collect();
                let names: Vec<_> = tokens
                    .windows(2)
                    .filter(|pair| pair[0].to_string() == "fn")
                    .map(|pair| pair[1].to_string())
                    .collect();
                assert!(!names.is_empty(), "proptest must contain named functions");
                (format!("proptest:{}", names.join(",")), "property")
            }
            syn::Item::Macro(m) if m.mac.path.is_ident("include") => {
                assert!(!fragment, "scenario fragments cannot include other files");
                assert!(
                    m.attrs.is_empty(),
                    "include statements must be unconditional and unadorned"
                );
                let path: syn::LitStr = syn::parse2(m.mac.tokens.clone())
                    .expect("include path must be a string literal");
                (format!("include:{}", path.value()), "include")
            }
            other => panic!("unsupported item: {}", other.to_token_stream()),
        };
        let span = item.span();
        let start = span.start();
        let end = span.end();
        items.push(json!({
            "name": name,
            "kind": kind,
            "start": offset(start),
            "end": offset(end),
        }));
    }
    println!("{}", json!(items));
}
