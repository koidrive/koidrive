// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Read};

use proc_macro2::TokenStream;
use quote::ToTokens;
use serde_json::json;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Lit, LitStr, Token};

fn string(expr: &Expr) -> bool {
    matches!(expr, Expr::Lit(literal) if matches!(literal.lit, Lit::Str(_)))
}

fn independent_of_source(path: &Expr) -> bool {
    let Expr::Macro(concat) = path else {
        return string(path);
    };
    let Ok(parts) = concat
        .mac
        .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
    else {
        return false;
    };
    let mut parts = parts.iter();
    concat.mac.path.is_ident("concat")
        && parts.next().is_some_and(|first| {
            matches!(first, Expr::Macro(env) if env.mac.path.is_ident("env")
                && env.mac.parse_body::<LitStr>().is_ok_and(|name| name.value() == "CARGO_MANIFEST_DIR"))
        })
        && parts.all(string)
}

fn persists_beside_source(tokens: TokenStream) -> bool {
    let Ok(attrs) = (|input: ParseStream| {
        let attrs = input.call(Attribute::parse_inner)?;
        input.parse::<TokenStream>()?;
        Ok(attrs)
    })
    .parse2(tokens) else {
        return true;
    };
    let Some(Ok(config)) = attrs
        .iter()
        .find(|attr| attr.path().is_ident("proptest_config"))
        .map(|attr| attr.parse_args::<Expr>())
    else {
        return true;
    };
    let base = match &config {
        Expr::Struct(update)
            if !update.fields.iter().any(|field| {
                matches!(&field.member, syn::Member::Named(name) if name == "failure_persistence")
            }) =>
        {
            update.rest.as_deref()
        }
        Expr::Struct(_) => None,
        other => Some(other),
    };
    let Some(Expr::Call(call)) = base else {
        return true;
    };
    let persisting = matches!(call.func.as_ref(), Expr::Path(function)
    if function.path.segments.last().is_some_and(|segment| {
        segment.ident == "battery_config" || segment.ident == "regression_config"
    }));
    !(persisting && call.args.first().is_some_and(independent_of_source))
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
        let anchored = matches!(&item, syn::Item::Macro(m)
            if kind == "property" && persists_beside_source(m.mac.tokens.clone()));
        let span = item.span();
        let start = span.start();
        let end = span.end();
        items.push(json!({
            "name": name,
            "kind": kind,
            "anchored": anchored,
            "start": offset(start),
            "end": offset(end),
        }));
    }
    println!("{}", json!(items));
}
