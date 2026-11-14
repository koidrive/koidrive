// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, PathArguments};

const CONFIG_HELP: &str = "use exactly one leading #![proptest_config(koidrive_test_support::battery_config(\"path/to/regressions.txt\", cases))]; only allowlisted deadline properties and the bounded runtime probe use regression_config; struct updates must not set cases or failure_persistence";
const DEADLINE_PATH: &str = "core/driver/src/folder/tests/unreadable_files.rs";
const DEADLINE_PROPERTIES: &[&str] = &[
    "files_that_cannot_be_read_hold_back_no_other_change",
    "files_that_cannot_be_written_hold_back_no_other_download",
];
const PROBE_PATH: &str = "core/test-support/tests/budgets.rs";

fn bounded_properties(path: &Path) -> Option<(&'static [&'static str], u32)> {
    match path.to_str()? {
        DEADLINE_PATH => Some((DEADLINE_PROPERTIES, 8)),
        PROBE_PATH => Some((&["bounded"], 4)),
        _ => None,
    }
}

struct Diagnostic {
    span: Span,
    message: String,
}

#[derive(Default)]
struct Inspection {
    blocks: usize,
    diagnostics: Vec<Diagnostic>,
    properties: Vec<String>,
}

struct Properties {
    config: Expr,
    names: Vec<String>,
}

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
        let tokens: Vec<_> = input.parse::<TokenStream>()?.into_iter().collect();
        let names = tokens
            .windows(2)
            .filter_map(|pair| match pair {
                [TokenTree::Ident(keyword), TokenTree::Ident(name)] if keyword == "fn" => {
                    Some(name.unraw().to_string())
                }
                _ => None,
            })
            .collect();
        Ok(Self { config, names })
    }
}

impl Properties {
    fn validate(&self, path: &Path) -> syn::Result<()> {
        let base = match &self.config {
            Expr::Struct(config) => {
                if config.fields.iter().any(|field| {
                    matches!(&field.member, syn::Member::Named(name) if name.unraw() == "failure_persistence" || name.unraw() == "cases")
                }) {
                    None
                } else {
                    config.rest.as_deref()
                }
            }
            other => Some(other),
        };
        let call = match base {
            Some(Expr::Call(call)) => match call.func.as_ref() {
                Expr::Path(function) if function.qself.is_none() => {
                    let segments = &function.path.segments;
                    (segments.len() == 2
                        && segments[0].ident.unraw() == "koidrive_test_support"
                        && matches!(
                            segments[1].ident.unraw().to_string().as_str(),
                            "regression_config" | "battery_config"
                        )
                        && segments
                            .iter()
                            .all(|segment| matches!(segment.arguments, PathArguments::None)))
                    .then_some(call)
                }
                _ => None,
            },
            _ => None,
        };
        let call = call.ok_or_else(|| syn::Error::new_spanned(
            &self.config,
            "config must directly call koidrive_test_support::battery_config or allowlisted regression_config, or use that call as a struct update base without setting cases or failure_persistence",
        ))?;
        let Expr::Path(function) = call.func.as_ref() else {
            unreachable!()
        };
        let bounded = function.path.segments[1].ident.unraw() == "regression_config";
        let exception = bounded_properties(path);
        if !bounded {
            if exception.is_some_and(|(names, _)| {
                self.names.iter().any(|name| names.contains(&name.as_str()))
            }) {
                return Err(syn::Error::new_spanned(
                    &self.config,
                    "allowlisted bounded properties must retain regression_config and their literal case budget",
                ));
            }
            return Ok(());
        }
        let cases = match exception {
            Some((names, cases))
                if self.names.len() == 1 && names.contains(&self.names[0].as_str()) =>
            {
                cases
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &self.config,
                    "regression_config requires exactly one allowlisted property at its exact source path; use battery_config for stress properties",
                ));
            }
        };
        let literal = match call.args.iter().nth(1) {
            Some(Expr::Lit(value)) => match &value.lit {
                syn::Lit::Int(value) => value.base10_parse::<u32>().ok(),
                _ => None,
            },
            _ => None,
        };
        if call.args.len() != 2 || literal != Some(cases) {
            return Err(syn::Error::new_spanned(
                &self.config,
                format!("allowlisted bounded property must use the literal case budget {cases}"),
            ));
        }
        Ok(())
    }
}

fn inspect_tokens(path: &Path, tokens: TokenStream, inspection: &mut Inspection) {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Ident(ident) = token
            && ident.unraw() == "proptest"
            && matches!(tokens.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
        {
            inspection.blocks += 1;
            match tokens.get(index + 2) {
                Some(TokenTree::Group(group)) => {
                    let validation =
                        syn::parse2::<Properties>(group.stream()).and_then(|properties| {
                            inspection
                                .properties
                                .extend(properties.names.iter().cloned());
                            properties.validate(path)
                        });
                    if let Err(error) = validation {
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
            inspect_tokens(path, group.stream(), inspection);
        }
    }
}

fn inspect_source(path: &Path, source: &str) -> Inspection {
    let mut inspection = Inspection::default();
    match syn::parse_file(source) {
        Ok(file) => {
            inspect_tokens(path, file.into_token_stream(), &mut inspection);
        }
        Err(error) => inspection.diagnostics.push(Diagnostic {
            span: error.span(),
            message: format!(
                "cannot parse Rust source: {error}; fix the syntax before checking persistence"
            ),
        }),
    }
    if let Some((names, _)) = bounded_properties(path) {
        for name in names {
            let count = inspection
                .properties
                .iter()
                .filter(|property| property.as_str() == *name)
                .count();
            if count != 1 {
                inspection.diagnostics.push(Diagnostic {
                    span: Span::call_site(),
                    message: format!(
                        "expected exactly one allowlisted bounded property {name}, found {count}"
                    ),
                });
            }
        }
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository root: {error}"))?;
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
        let canonical = file
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", file.display()))?;
        let inspection =
            inspect_source(canonical.strip_prefix(&root).unwrap_or(&canonical), &source);
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
        r##"#![proptest_config(koidrive_test_support::battery_config("regressions.txt", 32))]"##;
    const PROPERTY: &str =
        "#[test] fn identity(value in any::<u32>()) { prop_assert_eq!(value, value); }";

    fn errors(source: &str) -> Vec<Diagnostic> {
        inspect_source(Path::new("core/sim/tests/new.rs"), source).diagnostics
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
            "#![proptest_config(battery_config(\"regressions.txt\", 32))]",
            "#![proptest_config({ koidrive_test_support::battery_config(\"regressions.txt\", 32) })]",
            "#![proptest_config(koidrive_test_support::battery_config(\"regressions.txt\", 32).clone())]",
        ] {
            let diagnostics = errors(&property(config));
            assert_eq!(diagnostics.len(), 1, "{config}");
            assert!(diagnostics[0].message.contains("battery_config"));
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
            r##"#![proptest_config(Config { rng_seed: RngSeed::Fixed(42), ..koidrive_test_support::battery_config("regressions.txt", 32) })]"##.to_owned(),
        ] {
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
            ..koidrive_test_support::battery_config("regressions.txt", 32)
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
                "koidrive_test_support::battery_config(\"regressions.txt\", 32)",
                "ProptestConfig::default()",
            ),
            config.replace(
                "koidrive_test_support::battery_config(\"regressions.txt\", 32)",
                "helper()",
            ),
            config.replace(
                "koidrive_test_support::battery_config(\"regressions.txt\", 32)",
                "{ koidrive_test_support::battery_config(\"regressions.txt\", 32) }",
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
        let inspection = inspect_source(Path::new("core/sim/tests/new.rs"), source);
        assert!(inspection.diagnostics.is_empty());
        assert_eq!(inspection.blocks, 0);
    }

    #[test]
    fn discovered_block_count_includes_nested_and_qualified_invocations() {
        let source = format!(
            "proptest::proptest! {{ {CONFIG} fn outer(x in 0..1) {{ proptest! {{ {CONFIG} {PROPERTY} }} }} }}"
        );
        let inspection = inspect_source(Path::new("core/sim/tests/new.rs"), &source);
        assert!(inspection.diagnostics.is_empty());
        assert_eq!(inspection.blocks, 2);
        assert_eq!(
            inspect_source(Path::new("core/sim/tests/new.rs"), &property("")).blocks,
            1
        );
    }

    #[test]
    fn malformed_and_multiple_attributes_are_rejected() {
        for config in [
            format!("{CONFIG} {CONFIG}"),
            "#![proptest_config]".into(),
            "#![proptest_config()]".into(),
            "#![proptest_config(koidrive_test_support::battery_config(,))]".into(),
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

    fn bounded_block(name: &str, cases: &str) -> String {
        property(
            &CONFIG
                .replace("battery_config", "regression_config")
                .replace("32", cases),
        )
        .replace("identity", name)
    }

    fn deadlines() -> String {
        DEADLINE_PROPERTIES
            .iter()
            .map(|name| bounded_block(name, "8"))
            .collect()
    }

    #[test]
    fn new_sim_properties_require_stress_budgets() {
        assert!(errors(&property(CONFIG)).is_empty());
        let diagnostics = errors(&bounded_block("new_property", "32"));
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("use battery_config"));
        assert!(errors("fn replay() { let runner = regression_config(file, 0); }").is_empty());
    }

    #[test]
    fn deadline_exceptions_require_the_exact_source_path() {
        let source = deadlines();
        assert!(
            inspect_source(Path::new(DEADLINE_PATH), &source)
                .diagnostics
                .is_empty()
        );
        for path in [
            "core/sim/tests/new.rs",
            "core/driver/src/folder/tests/renamed.rs",
            "other/core/driver/src/folder/tests/unreadable_files.rs",
        ] {
            let diagnostics = inspect_source(Path::new(path), &source).diagnostics;
            assert_eq!(diagnostics.len(), 2, "{path}");
        }
        let stress = source.replace("regression_config", "battery_config");
        assert!(errors(&stress).is_empty());
    }

    #[test]
    fn deadline_exceptions_require_a_literal_eight_case_budget() {
        for budget in ["0", "4", "9", "CASES", "8 + 0", "{ 8 }"] {
            let source = deadlines().replace(
                "regression_config(\"regressions.txt\", 8)",
                &format!("regression_config(\"regressions.txt\", {budget})"),
            );
            let diagnostics = inspect_source(Path::new(DEADLINE_PATH), &source).diagnostics;
            assert_eq!(diagnostics.len(), 2, "{budget}");
            assert!(
                diagnostics
                    .iter()
                    .all(|error| error.message.contains("literal case budget 8"))
            );
        }
    }

    #[test]
    fn bounded_blocks_cannot_admit_missing_renamed_or_extra_properties() {
        let source = deadlines();
        for changed in [
            source.replace(&PROPERTY.replace("identity", DEADLINE_PROPERTIES[0]), ""),
            source.replace(DEADLINE_PROPERTIES[0], "renamed"),
            source.replace("#[test] fn", "#[test] fn extra(x in 0..1) {} #[test] fn"),
            source.replace(DEADLINE_PROPERTIES[0], DEADLINE_PROPERTIES[1]),
            bounded_block(DEADLINE_PROPERTIES[0], "8"),
        ] {
            assert!(
                !inspect_source(Path::new(DEADLINE_PATH), &changed)
                    .diagnostics
                    .is_empty(),
                "{changed}"
            );
        }
        let extra_stress = format!("{source} {}", property(CONFIG));
        assert!(
            inspect_source(Path::new(DEADLINE_PATH), &extra_stress)
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn deadline_properties_cannot_switch_to_battery_budgets() {
        let source = deadlines().replace("regression_config", "battery_config");
        let diagnostics = inspect_source(Path::new(DEADLINE_PATH), &source).diagnostics;
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .all(|error| error.message.contains("must retain regression_config"))
        );
    }

    #[test]
    fn bounded_probe_is_separate_from_production_deadlines() {
        let source = bounded_block("bounded", "4");
        assert!(
            inspect_source(Path::new(PROBE_PATH), &source)
                .diagnostics
                .is_empty()
        );
        assert!(
            !inspect_source(Path::new(DEADLINE_PATH), &source)
                .diagnostics
                .is_empty()
        );
        assert!(
            !inspect_source(
                Path::new("other/core/test-support/tests/budgets.rs"),
                &source
            )
            .diagnostics
            .is_empty()
        );
        for changed in [
            source.replace("bounded", "other"),
            source.replace(
                "regression_config(\"regressions.txt\", 4)",
                "regression_config(\"regressions.txt\", 8)",
            ),
            source.replace("regression_config", "battery_config"),
            source.replace(&PROPERTY.replace("identity", "bounded"), ""),
            format!("{source}\n{source}"),
        ] {
            assert!(
                !inspect_source(Path::new(PROBE_PATH), &changed)
                    .diagnostics
                    .is_empty(),
                "{changed}"
            );
        }
        assert!(
            !inspect_source(Path::new(PROBE_PATH), &deadlines())
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn struct_updates_cannot_bypass_case_budgets() {
        for field in ["cases: 0", "cases", "r#cases: 8"] {
            let config = format!(
                "#![proptest_config(Config {{ {field}, ..koidrive_test_support::battery_config(\"regressions.txt\", 32) }})]"
            );
            assert_eq!(errors(&property(&config)).len(), 1, "{field}");
            let source = deadlines().replace(
                "koidrive_test_support::regression_config(\"regressions.txt\", 8)",
                &format!("Config {{ {field}, ..koidrive_test_support::regression_config(\"regressions.txt\", 8) }}"),
            );
            assert_eq!(
                inspect_source(Path::new(DEADLINE_PATH), &source)
                    .diagnostics
                    .len(),
                2,
                "{field}"
            );
        }
    }
}
