// SPDX-License-Identifier: Apache-2.0

mod test_spans;

use std::io::Read;
use std::process::ExitCode;

use quote::ToTokens;
use syn::visit::{self, Visit};
use syn::{
    AttrStyle, Attribute, Expr, ExprLit, ImplItemFn, ItemFn, ItemImpl, ItemMod, ItemTrait, Lit,
    Meta, Signature, TraitItemFn, Visibility,
};

#[derive(Default)]
struct Fns {
    scope: Vec<String>,
    public_trait: bool,
    trait_impl: bool,
    found: Vec<String>,
}

impl Fns {
    fn add(&mut self, attrs: &[Attribute], public: bool, sig: &Signature) {
        let docs: Vec<(usize, String)> = attrs
            .iter()
            .filter(|attr| matches!(attr.style, AttrStyle::Outer))
            .filter_map(|attr| {
                let Meta::NameValue(meta) = &attr.meta else {
                    return None;
                };
                let Expr::Lit(ExprLit {
                    lit: Lit::Str(text),
                    ..
                }) = &meta.value
                else {
                    return None;
                };
                meta.path
                    .is_ident("doc")
                    .then(|| (attr.pound_token.span.start().line, text.value()))
            })
            .collect();
        let line = sig.fn_token.span.start().line;
        let top = docs.first().map_or(line, |(top, _)| *top);
        let text: Vec<&str> = docs
            .iter()
            .flat_map(|(_, doc)| doc.lines())
            .map(|line| line.trim().trim_start_matches('*').trim())
            .filter(|line| !line.is_empty())
            .collect();
        self.found.push(format!(
            "{}\t{}\t{public}\t{line}\t{top}\t{}",
            self.scope.join("::").escape_default(),
            sig.ident,
            text.join("\n").escape_default()
        ));
    }

    fn within(&mut self, scope: String, visit: impl FnOnce(&mut Self)) {
        self.scope.push(scope);
        visit(self);
        self.scope.pop();
    }
}

impl<'a> Visit<'a> for Fns {
    fn visit_item_mod(&mut self, item: &'a ItemMod) {
        self.within(format!("mod {}", item.ident), |fns| {
            visit::visit_item_mod(fns, item)
        });
    }

    fn visit_item_impl(&mut self, item: &'a ItemImpl) {
        let of = item
            .trait_
            .as_ref()
            .map(|(_, path, _)| format!("{} for ", path.to_token_stream()))
            .unwrap_or_default();
        let scope = format!("impl {of}{}", item.self_ty.to_token_stream());
        let outer = std::mem::replace(&mut self.trait_impl, item.trait_.is_some());
        self.within(scope, |fns| visit::visit_item_impl(fns, item));
        self.trait_impl = outer;
    }

    fn visit_item_trait(&mut self, item: &'a ItemTrait) {
        let public = matches!(item.vis, Visibility::Public(_));
        let outer = std::mem::replace(&mut self.public_trait, public);
        self.within(format!("trait {}", item.ident), |fns| {
            visit::visit_item_trait(fns, item)
        });
        self.public_trait = outer;
    }

    fn visit_item_fn(&mut self, item: &'a ItemFn) {
        self.add(
            &item.attrs,
            matches!(item.vis, Visibility::Public(_)),
            &item.sig,
        );
        self.within(format!("fn {}", item.sig.ident), |fns| {
            visit::visit_item_fn(fns, item)
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'a ImplItemFn) {
        let public = self.trait_impl || matches!(item.vis, Visibility::Public(_));
        self.add(&item.attrs, public, &item.sig);
        self.within(format!("fn {}", item.sig.ident), |fns| {
            visit::visit_impl_item_fn(fns, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'a TraitItemFn) {
        self.add(&item.attrs, self.public_trait, &item.sig);
        self.within(format!("fn {}", item.sig.ident), |fns| {
            visit::visit_trait_item_fn(fns, item)
        });
    }
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("test-spans") {
        return match test_spans::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    let mut src = String::new();
    if std::io::stdin().read_to_string(&mut src).is_err() {
        return ExitCode::FAILURE;
    }
    let Ok(file) = syn::parse_file(&src) else {
        return ExitCode::FAILURE;
    };
    let mut fns = Fns::default();
    fns.visit_file(&file);
    for line in fns.found {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
