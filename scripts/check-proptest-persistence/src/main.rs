// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, PathArguments};

const CONFIG_HELP: &str = "use exactly one leading #![proptest_config(koidrive_test_support::regression_config(\"path/to/regressions.txt\", cases))], or opt in to stress budgets with battery_config; struct updates must use that base without setting failure_persistence";

struct Diagnostic {
    span: Span,
    message: String,
}

#[derive(Default)]
struct Inspection {
    blocks: usize,
    diagnostics: Vec<Diagnostic>,
}

struct Properties;

impl Parse for Properties {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_inner)?;
        let configs: Vec<_> = attrs
            .iter()
            .filter(|attr| attr.path().is_ident("proptest_config"))
            .collect();
        if configs.len() != 1 {
            return Err(input.error(format!(
                "expected exactly one leading proptest_config attribute, found {}",
                configs.len()
            )));
        }
        let config = configs[0].parse_args::<Expr>()?;
        let base = match &config {
            Expr::Struct(config) => {
                if config.fields.iter().any(|field| {
                    matches!(&field.member, syn::Member::Named(name) if name.unraw() == "failure_persistence")
                }) {
                    None
                } else {
                    config.rest.as_deref()
                }
            }
            other => Some(other),
        };
        let valid = match base {
            Some(Expr::Call(call)) => match call.func.as_ref() {
                Expr::Path(function) if function.qself.is_none() => {
                    let segments = &function.path.segments;
                    segments.len() == 2
                        && segments[0].ident.unraw() == "koidrive_test_support"
                        && matches!(
                            segments[1].ident.unraw().to_string().as_str(),
                            "regression_config" | "battery_config"
                        )
                        && segments
                            .iter()
                            .all(|segment| matches!(segment.arguments, PathArguments::None))
                }
                _ => false,
            },
            _ => false,
        };
        if !valid {
            return Err(syn::Error::new_spanned(
                config,
                "config must directly call koidrive_test_support::regression_config or koidrive_test_support::battery_config, or use that call as a struct update base without setting failure_persistence",
            ));
        }
        input.parse::<TokenStream>()?;
        Ok(Self)
    }
}

fn inspect_tokens(tokens: TokenStream, inspection: &mut Inspection) {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Ident(ident) = token
            && ident.unraw() == "proptest"
            && matches!(tokens.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
        {
            inspection.blocks += 1;
            match tokens.get(index + 2) {
                Some(TokenTree::Group(group)) => {
                    if let Err(error) = syn::parse2::<Properties>(group.stream()) {
                        inspection.diagnostics.push(Diagnostic {
                            span: ident.span().join(group.span()).unwrap_or(ident.span()),
                            message: format!("invalid proptest!: {error}; {CONFIG_HELP}"),
                        });
                    }
                }
                _ => inspection.diagnostics.push(Diagnostic {
                    span: ident.span(),
                    message: format!("malformed proptest!: expected a token group; {CONFIG_HELP}"),
                }),
            }
        }
        if let TokenTree::Group(group) = token {
            inspect_tokens(group.stream(), inspection);
        }
    }
}

fn inspect_source(source: &str) -> Inspection {
    let mut inspection = Inspection::default();
    match syn::parse_file(source) {
        Ok(file) => {
            inspect_tokens(file.into_token_stream(), &mut inspection);
        }
        Err(error) => inspection.diagnostics.push(Diagnostic {
            span: error.span(),
            message: format!(
                "cannot parse Rust source: {error}; fix the syntax before checking persistence"
            ),
        }),
    }
    inspection
}

fn tracked_files() -> Result<Vec<PathBuf>, String> {
    let root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("cannot run git rev-parse: {error}"))?;
    if !root.status.success() {
        return Err(
            "git rev-parse failed; run inside a repository or supply Rust file paths".into(),
        );
    }
    let root = std::str::from_utf8(&root.stdout)
        .map_err(|error| format!("repository path is not UTF-8: {error}"))?
        .trim_end_matches('\n');
    let files = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()
        .map_err(|error| format!("cannot run git ls-files: {error}"))?;
    if !files.status.success() {
        return Err(format!("git ls-files failed with {}", files.status));
    }
    files
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(|path| PathBuf::from(root).join(path))
                .map_err(|error| format!("tracked Rust path is not UTF-8: {error}"))
        })
        .collect()
}

fn run() -> Result<bool, String> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == OsStr::new("--")) {
        args.next();
    }
    let mut files: Vec<_> = args.map(PathBuf::from).collect();
    let default_scan = files.is_empty();
    if default_scan {
        files = tracked_files()?;
    }
    files.sort();
    files.dedup();
    let mut valid = true;
    let mut blocks = 0;
    for file in files {
        let source = match fs::read_to_string(&file) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("{}:1:1: cannot read Rust source: {error}", file.display());
                valid = false;
                continue;
            }
        };
        let inspection = inspect_source(&source);
        blocks += inspection.blocks;
        for diagnostic in inspection.diagnostics {
            let start = diagnostic.span.start();
            let end = diagnostic.span.end();
            eprintln!(
                "{}:{}:{}-{}:{}: {}",
                file.display(),
                start.line,
                start.column + 1,
                end.line,
                end.column + 1,
                diagnostic.message
            );
            valid = false;
        }
    }
    if default_scan && blocks == 0 {
        return Err(
            "repository scan found no proptest! blocks; verify tracked Rust file discovery".into(),
        );
    }
    if default_scan && valid {
        println!("Checked {blocks} proptest! blocks.");
    }
    Ok(valid)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("check-proptest-persistence: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str =
        r##"#![proptest_config(koidrive_test_support::regression_config("regressions.txt", 32))]"##;
    const PROPERTY: &str =
        "#[test] fn identity(value in any::<u32>()) { prop_assert_eq!(value, value); }";

    fn errors(source: &str) -> Vec<Diagnostic> {
        inspect_source(source).diagnostics
    }

    fn property(config: &str) -> String {
        format!("proptest! {{ {config} {PROPERTY} }}")
    }

    #[test]
    fn default_persistence_is_rejected() {
        for config in [
            "",
            "#![proptest_config(Config::default())]",
            "#![proptest_config(Config::with_cases(32))]",
            "#![proptest_config(helper())]",
            "#![proptest_config(regression_config(\"regressions.txt\", 32))]",
            "#![proptest_config({ koidrive_test_support::regression_config(\"regressions.txt\", 32) })]",
            "#![proptest_config(koidrive_test_support::regression_config(\"regressions.txt\", 32).clone())]",
        ] {
            let diagnostics = errors(&property(config));
            assert_eq!(diagnostics.len(), 1, "{config}");
            assert!(diagnostics[0].message.contains("regression_config"));
        }
    }

    #[test]
    fn explicit_config_passes() {
        assert!(errors(&property(CONFIG)).is_empty());
        assert!(
            errors(&property(&CONFIG.replace(
                "koidrive_test_support::",
                "::koidrive_test_support::"
            )))
            .is_empty()
        );
    }

    #[test]
    fn battery_opt_in_preserves_persistence_guarantees() {
        for config in [
            CONFIG.to_owned(),
            r##"#![proptest_config(Config { rng_seed: RngSeed::Fixed(42), ..koidrive_test_support::regression_config("regressions.txt", 32) })]"##.to_owned(),
        ] {
            let config = config.replace("regression_config", "battery_config");
            assert!(errors(&property(&config)).is_empty());
            assert!(errors(&property(&config.replace("koidrive_test_support::", "::koidrive_test_support::"))).is_empty());
            for invalid in [
                config.replace("koidrive_test_support::", ""),
                config.replace("battery_config", "other_config"),
                format!("{config} {config}"),
                config.replace("rng_seed: RngSeed::Fixed(42)", "failure_persistence: None"),
                config.replace("rng_seed: RngSeed::Fixed(42)", "r#failure_persistence"),
            ] {
                if invalid != config {
                    assert_eq!(errors(&property(&invalid)).len(), 1, "{invalid}");
                }
            }
        }
    }

    #[test]
    fn struct_update_preserves_explicit_persistence() {
        let config = r##"#![proptest_config(ProptestConfig {
            rng_seed: RngSeed::Fixed(42),
            ..koidrive_test_support::regression_config("regressions.txt", 32)
        })]"##;
        assert!(errors(&property(config)).is_empty());
        for config in [
            config.replace("rng_seed: RngSeed::Fixed(42)", "failure_persistence: None"),
            config.replace("rng_seed: RngSeed::Fixed(42)", "failure_persistence"),
            config.replace(
                "rng_seed: RngSeed::Fixed(42)",
                "r#failure_persistence: None",
            ),
            config.replace(
                "koidrive_test_support::regression_config(\"regressions.txt\", 32)",
                "ProptestConfig::default()",
            ),
            config.replace(
                "koidrive_test_support::regression_config(\"regressions.txt\", 32)",
                "helper()",
            ),
            config.replace(
                "koidrive_test_support::regression_config(\"regressions.txt\", 32)",
                "{ koidrive_test_support::regression_config(\"regressions.txt\", 32) }",
            ),
            "#![proptest_config(ProptestConfig { rng_seed: RngSeed::Fixed(42) })]".into(),
        ] {
            assert_eq!(errors(&property(&config)).len(), 1, "{config}");
        }
    }

    #[test]
    fn qualified_multiline_and_nested_invocations_are_checked() {
        let source = format!(
            "mod tests {{\n proptest::\n proptest! {{ {PROPERTY} }}\n wrapper! {{\n ::proptest::proptest! {{ {PROPERTY} }}\n }}\n }}"
        );
        let diagnostics = errors(&source);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].span.start().line, 3);
        assert_eq!(diagnostics[1].span.start().line, 5);
        let valid = source.replace(PROPERTY, &format!("{CONFIG} {PROPERTY}"));
        assert!(errors(&valid).is_empty());
    }

    #[test]
    fn nested_properties_inside_property_bodies_are_checked() {
        let source = format!(
            "proptest! {{ {CONFIG} fn outer(x in 0..1) {{ proptest! {{ {PROPERTY} }} }} }}"
        );
        assert_eq!(errors(&source).len(), 1);
    }

    #[test]
    fn raw_identifier_invocation_is_checked() {
        assert_eq!(
            errors(&property("").replace("proptest!", "r#proptest!")).len(),
            1
        );
    }

    #[test]
    fn comments_and_strings_are_ignored() {
        let source = r####"
            // proptest! { invalid }
            /* proptest! { /* nested comment */ invalid } */
            const TEXT: &str = "proptest! { invalid }";
            const RAW: &str = r###"proptest! { invalid }"###;
            /// proptest! { invalid }
            fn ordinary() {}
        "####;
        let inspection = inspect_source(source);
        assert!(inspection.diagnostics.is_empty());
        assert_eq!(inspection.blocks, 0);
    }

    #[test]
    fn discovered_block_count_includes_nested_and_qualified_invocations() {
        let source = format!(
            "proptest::proptest! {{ {CONFIG} fn outer(x in 0..1) {{ proptest! {{ {CONFIG} {PROPERTY} }} }} }}"
        );
        let inspection = inspect_source(&source);
        assert!(inspection.diagnostics.is_empty());
        assert_eq!(inspection.blocks, 2);
        assert_eq!(inspect_source(&property("")).blocks, 1);
    }

    #[test]
    fn malformed_and_multiple_attributes_are_rejected() {
        for config in [
            format!("{CONFIG} {CONFIG}"),
            "#![proptest_config]".into(),
            "#![proptest_config()]".into(),
            "#![proptest_config(koidrive_test_support::regression_config(,))]".into(),
        ] {
            assert_eq!(errors(&property(&config)).len(), 1, "{config}");
        }
    }

    #[test]
    fn malformed_invocations_and_rust_are_rejected() {
        for source in ["wrapper! { proptest! ; }", "proptest! {", "fn broken( {}"] {
            assert_eq!(errors(source).len(), 1, "{source}");
        }
    }

    #[test]
    fn unrelated_inner_attributes_do_not_change_persistence_validation() {
        assert!(errors(&property(&format!("#![allow(dead_code)] {CONFIG}"))).is_empty());
    }

    #[test]
    fn shebang_and_file_attributes_preserve_source_lines() {
        let source = format!(
            "#!/usr/bin/env rust-script\n#![allow(dead_code)]\n{}",
            property("")
        );
        let diagnostics = errors(&source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].span.start().line, 3);
    }
}
