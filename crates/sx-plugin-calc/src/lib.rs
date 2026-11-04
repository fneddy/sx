//! # `sx-plugin-calc`
//!
//! Built-in calculator handler for the `calc:` and `=:` namespaces.
//!
//! Uses [`exmex`] (MIT OR Apache-2.0) for expression evaluation — supports
//! arithmetic, `sqrt`, `sin`, `cos`, `tan`, `pi`, `e`, and exponentiation.
//!
//! ## Expressions
//!
//! ```text
//! calc:2^32        → 4294967296
//! calc:sqrt(2)     → 1.4142135623730951
//! calc:sin(pi/2)   → 1
//! =:1920/1080      → 1.7777777777777777
//! ```

use exmex::{Express, FlatEx, FloatOpsFactory, MakeOperators, Operator};
use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxMetaEntry, SxNamespacePolicy, SxQueryContext, SxResult,
    SxResultSender,
};

/// Stateless calc handler — all state lives in the expression string.
pub struct CalcHandler;

impl CalcHandler {
    /// Initialise this plugin's logger. Safe to call multiple times.
    pub fn init_logger() {
        let _ = env_logger::try_init();
    }
}

impl sx_plugin_api::SxHandler for CalcHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        let mut aliases = stabby::vec::Vec::new();
        aliases.push(SxBytes::from_str_bytes("="));

        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("calc"),
            aliases,
            description: SxBytes::from_str_bytes(
                "Evaluate mathematical expressions (calc:2^32, =:sqrt(2)*pi)",
            ),
            value_hint: SxBytes::from_str_bytes("2 + 2"),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: false,
            is_file_index: false,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        let expr = value.as_ref(); // &str
        log::debug!("calc: evaluating {:?}", expr);
        let result_str = evaluate(expr);
        log::debug!("calc: result = {:?}", result_str);
        let mut result = SxResult::new("calc", 1.0);
        result.metadata.push(SxMetaEntry::text("value", &result_str));
        sender.send(result);
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(
        &self,
        _partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        // Always show fixed examples — calc is free-form, no meaningful completion.
        const EXAMPLES: &[(&str, &str)] = &[
            ("2 + 2", "2 + 2"),
            ("2^32", "2^32"),
            ("sqrt(2)", "sqrt(2)"),
            ("sin(pi/2)", "sin(pi/2)"),
        ];
        let mut items = stabby::vec::Vec::new();
        for &(label, completion) in EXAMPLES {
            items.push(sx_plugin_api::SxHintEntry {
                label: SxBytes::from_str_bytes(label),
                completion: SxBytes::from_str_bytes(completion),
            });
        }
        sx_plugin_api::SxHints { items }
    }
}

/// Extends exmex's `FloatOpsFactory` with lowercase `pi` and `tau` so users
/// can write `calc:sin(pi/2)` without capitalisation. exmex already provides
/// `PI`, `π`, `TAU`, `τ`, `E`, `e`.
// Clone is required by exmex's MakeOperators trait bound.
#[derive(Clone, Debug)]
struct SxOpsFactory;

impl MakeOperators<f64> for SxOpsFactory {
    fn make<'a>() -> Vec<Operator<'a, f64>> {
        let mut ops = FloatOpsFactory::<f64>::make();
        ops.push(Operator::make_constant("pi", std::f64::consts::PI));
        ops.push(Operator::make_constant("tau", std::f64::consts::TAU));
        ops
    }
}

/// Evaluate an expression string and return a human-readable result.
///
/// Returns `"error: <message>"` on parse error. IEEE 754 infinities and NaN
/// are mapped to human-readable strings.
pub fn evaluate(expr: &str) -> String {
    // Reject long input early: the regex-based parser can be O(n²) on
    // pathological operator sequences.
    if expr.len() > 1_000 {
        log::warn!(
            "calc: expression too long ({} chars), rejecting",
            expr.len()
        );
        return "error: expression too long (max 1000 chars)".to_owned();
    }

    // parse_wo_compile is cheaper than parse for one-shot evaluation.
    let flat_ex = match FlatEx::<f64, SxOpsFactory>::parse_wo_compile(expr) {
        Ok(e) => e,
        Err(e) => {
            log::debug!("calc: parse error for {:?}: {}", expr, e.msg());
            return format!("error: {}", e.msg());
        }
    };
    if !flat_ex.var_names().is_empty() {
        log::debug!(
            "calc: unknown identifiers in {:?}: {:?}",
            expr,
            flat_ex.var_names()
        );
        return format!(
            "error: unknown identifier{}: {}",
            if flat_ex.var_names().len() == 1 {
                ""
            } else {
                "s"
            },
            flat_ex.var_names().join(", ")
        );
    }
    match flat_ex.eval(&[]) {
        Ok(val) => format_value(val),
        Err(e) => {
            log::debug!("calc: eval error for {:?}: {}", expr, e.msg());
            format!("error: {}", e.msg())
        }
    }
}

fn format_value(val: f64) -> String {
    if val.is_nan() {
        return "NaN".to_owned();
    }
    if val.is_infinite() {
        return if val > 0.0 {
            "∞ (result is infinite — divide by zero?)".to_owned()
        } else {
            "-∞ (result is infinite — divide by zero?)".to_owned()
        };
    }
    // Integers up to 2^53 are exactly representable in f64 and fit in i64.
    if val.fract() == 0.0 && val.abs() < 9.007_199_254_740_992e15 {
        format!("{}", val as i64)
    } else {
        format!("{val}")
    }
}

sx_plugin_api::declare_plugin!(CalcHandler);
