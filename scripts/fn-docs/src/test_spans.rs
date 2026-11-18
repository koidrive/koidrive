// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, Read, Write};

use proc_macro2::Span;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Arm, AttrStyle, Attribute, Expr, ExprLit, Field, FieldValue, ForeignItem, ImplItem, Item,
    ItemMod, Lit, Local, Meta, StmtMacro, Token, TraitItem, Variant,
};

#[derive(Default)]
struct Spans {
    whole_file: bool,
    ranges: Vec<(usize, usize)>,
    modules: Vec<String>,
    inline: Vec<String>,
    in_test: usize,
}

fn test_only_predicate(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|args| args.iter().any(test_only_predicate)),
        _ => false,
    }
}

fn test_only(attr: &Attribute) -> bool {
    match &attr.meta {
        Meta::Path(path) => path
            .segments
            .last()
            .is_some_and(|last| last.ident == "test"),
        Meta::List(list) if list.path.is_ident("cfg") => list
            .parse_args::<Meta>()
            .is_ok_and(|meta| test_only_predicate(&meta)),
        Meta::List(list) => list
            .path
            .segments
            .last()
            .is_some_and(|last| last.ident == "test"),
        _ => false,
    }
}

fn path_override(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        let Meta::NameValue(meta) = &attr.meta else {
            return None;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(path),
            ..
        }) = &meta.value
        else {
            return None;
        };
        meta.path.is_ident("path").then(|| path.value())
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn foreign_item_attrs(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(item) => &item.attrs,
        ForeignItem::Static(item) => &item.attrs,
        ForeignItem::Type(item) => &item.attrs,
        ForeignItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(e) => &e.attrs,
        Expr::Assign(e) => &e.attrs,
        Expr::Async(e) => &e.attrs,
        Expr::Await(e) => &e.attrs,
        Expr::Binary(e) => &e.attrs,
        Expr::Block(e) => &e.attrs,
        Expr::Break(e) => &e.attrs,
        Expr::Call(e) => &e.attrs,
        Expr::Cast(e) => &e.attrs,
        Expr::Closure(e) => &e.attrs,
        Expr::Const(e) => &e.attrs,
        Expr::Continue(e) => &e.attrs,
        Expr::Field(e) => &e.attrs,
        Expr::ForLoop(e) => &e.attrs,
        Expr::Group(e) => &e.attrs,
        Expr::If(e) => &e.attrs,
        Expr::Index(e) => &e.attrs,
        Expr::Infer(e) => &e.attrs,
        Expr::Let(e) => &e.attrs,
        Expr::Lit(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Paren(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Range(e) => &e.attrs,
        Expr::RawAddr(e) => &e.attrs,
        Expr::Reference(e) => &e.attrs,
        Expr::Repeat(e) => &e.attrs,
        Expr::Return(e) => &e.attrs,
        Expr::Struct(e) => &e.attrs,
        Expr::Try(e) => &e.attrs,
        Expr::TryBlock(e) => &e.attrs,
        Expr::Tuple(e) => &e.attrs,
        Expr::Unary(e) => &e.attrs,
        Expr::Unsafe(e) => &e.attrs,
        Expr::While(e) => &e.attrs,
        Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

impl Spans {
    fn mark(&mut self, attrs: &[Attribute], span: Span) -> bool {
        let outer = attrs
            .iter()
            .filter(|attr| matches!(attr.style, AttrStyle::Outer));
        let marked = outer.clone().any(test_only);
        if marked {
            let start = outer
                .map(|attr| attr.span().start().line)
                .min()
                .unwrap_or(span.start().line);
            self.ranges
                .push((start.min(span.start().line), span.end().line));
        }
        marked
    }

    fn module(&mut self, item: &ItemMod) {
        let test = self.mark(&item.attrs, item.span());
        let path = path_override(&item.attrs);
        if item.content.is_none() {
            self.modules.push(format!(
                "{}\t{}\t{}\t{}",
                u8::from(test || self.in_test > 0),
                item.ident,
                path.as_deref().unwrap_or("-"),
                self.inline.join("\u{1f}")
            ));
            return;
        }
        self.inline
            .push(path.unwrap_or_else(|| item.ident.to_string()));
        self.in_test += usize::from(test);
        visit::visit_item_mod(self, item);
        self.in_test -= usize::from(test);
        self.inline.pop();
    }
}

impl<'a> Visit<'a> for Spans {
    fn visit_item(&mut self, item: &'a Item) {
        if let Item::Mod(module) = item {
            return self.module(module);
        }
        self.mark(item_attrs(item), item.span());
        visit::visit_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'a ImplItem) {
        self.mark(impl_item_attrs(item), item.span());
        visit::visit_impl_item(self, item);
    }

    fn visit_trait_item(&mut self, item: &'a TraitItem) {
        self.mark(trait_item_attrs(item), item.span());
        visit::visit_trait_item(self, item);
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        self.mark(foreign_item_attrs(item), item.span());
        visit::visit_foreign_item(self, item);
    }

    fn visit_field(&mut self, field: &'a Field) {
        self.mark(&field.attrs, field.span());
        visit::visit_field(self, field);
    }

    fn visit_field_value(&mut self, field: &'a FieldValue) {
        self.mark(&field.attrs, field.span());
        visit::visit_field_value(self, field);
    }

    fn visit_variant(&mut self, variant: &'a Variant) {
        self.mark(&variant.attrs, variant.span());
        visit::visit_variant(self, variant);
    }

    fn visit_arm(&mut self, arm: &'a Arm) {
        self.mark(&arm.attrs, arm.span());
        visit::visit_arm(self, arm);
    }

    fn visit_local(&mut self, local: &'a Local) {
        self.mark(&local.attrs, local.span());
        visit::visit_local(self, local);
    }

    fn visit_stmt_macro(&mut self, stmt: &'a StmtMacro) {
        self.mark(&stmt.attrs, stmt.span());
        visit::visit_stmt_macro(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        self.mark(expr_attrs(expr), expr.span());
        visit::visit_expr(self, expr);
    }
}

fn scan(src: &str, out: &mut impl Write) -> std::io::Result<()> {
    let file = match syn::parse_file(src) {
        Ok(file) => file,
        Err(error) => return writeln!(out, "error\t{}:{}", error.span().start().line, error),
    };
    let mut spans = Spans {
        whole_file: file
            .attrs
            .iter()
            .any(|attr| matches!(attr.style, AttrStyle::Inner(_)) && test_only(attr)),
        ..Spans::default()
    };
    spans.visit_file(&file);
    if spans.whole_file {
        writeln!(out, "whole")?;
    }
    for (start, end) in spans.ranges {
        writeln!(out, "range\t{start}\t{end}")?;
    }
    for module in spans.modules {
        writeln!(out, "mod\t{module}")?;
    }
    Ok(())
}

pub fn run() -> std::io::Result<()> {
    let mut input = std::io::stdin().lock();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut header = String::new();
    while input.read_line(&mut header)? > 0 {
        let (id, len) = header.trim_end().rsplit_once(' ').unwrap_or_default();
        let mut bytes = vec![0; len.parse().unwrap_or(0)];
        input.read_exact(&mut bytes)?;
        writeln!(out, "file\t{id}")?;
        scan(&String::from_utf8_lossy(&bytes), &mut out)?;
        writeln!(out, "end")?;
        header.clear();
    }
    out.flush()
}
