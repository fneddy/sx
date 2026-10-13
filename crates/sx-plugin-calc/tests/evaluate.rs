use std::sync::{Arc, Mutex};

use sx_plugin_api::{SxHandler as _, SxQueryContext, SxResultSender};
use sx_plugin_calc::{evaluate, CalcHandler};

#[test]
fn power_of_two() {
    assert_eq!(evaluate("2^32"), "4294967296");
}

#[test]
fn sqrt_two() {
    let r = evaluate("sqrt(2)");
    let v: f64 = r.parse().unwrap();
    let expected = std::f64::consts::SQRT_2;
    assert!((v - expected).abs() < 1e-10, "got {r}");
}

#[test]
fn simple_division() {
    let r = evaluate("1920/1080");
    let v: f64 = r.parse().unwrap();
    assert!((v - (1920.0_f64 / 1080.0)).abs() < 1e-10, "got {r}");
}

#[test]
fn integer_result_has_no_decimal() {
    assert_eq!(evaluate("6*7"), "42");
}

#[test]
fn pi_constant_lowercase() {
    let r = evaluate("pi");
    let v: f64 = r.parse().unwrap();
    assert!((v - std::f64::consts::PI).abs() < 1e-10, "got {r}");
}

#[test]
fn pi_uppercase_still_works() {
    let r = evaluate("PI");
    let v: f64 = r.parse().unwrap();
    assert!((v - std::f64::consts::PI).abs() < 1e-10, "got {r}");
}

#[test]
fn tau_constant_lowercase() {
    let r = evaluate("tau");
    let v: f64 = r.parse().unwrap();
    assert!((v - std::f64::consts::TAU).abs() < 1e-10, "got {r}");
}

#[test]
fn sin_of_pi_over_two() {
    let r = evaluate("sin(pi/2)");
    let v: f64 = r.parse().unwrap();
    assert!((v - 1.0).abs() < 1e-10, "got {r}");
}

#[test]
fn div_by_zero_returns_infinity_message() {
    let r = evaluate("1/0");
    assert!(r.starts_with('∞'), "got {r}");
}

#[test]
fn sqrt_negative_returns_nan_message() {
    let r = evaluate("sqrt(-1)");
    assert!(r.starts_with("NaN"), "got {r}");
}

#[test]
fn unknown_identifier_returns_error() {
    let r = evaluate("foo + 1");
    assert!(r.starts_with("error:"), "got {r}");
    assert!(r.contains("foo"), "got {r}");
}

#[test]
fn syntax_error_returns_error() {
    // Unclosed parenthesis — genuine parse error in exmex.
    let r = evaluate("(2+3");
    assert!(r.starts_with("error:"), "got {r}");
}

#[test]
fn large_exponent_overflows_to_infinity() {
    let r = evaluate("2^1024");
    assert!(r.starts_with('∞'), "got {r}");
}

#[test]
fn integer_at_2_pow_53_exact() {
    assert_eq!(evaluate("9007199254740992"), "9007199254740992");
}

#[test]
fn input_too_long_returns_error() {
    let long = "1+".repeat(501);
    let r = evaluate(&long);
    assert!(r.starts_with("error:"), "got {r}");
}

#[test]
fn handler_info_namespace() {
    let h = CalcHandler;
    let info = h.info();
    assert_eq!(info.namespace.as_str(), "calc");
    assert_eq!(info.aliases.as_slice().len(), 1);
    assert_eq!(info.aliases.as_slice()[0].as_str(), "=");
}

#[test]
fn handler_query_sends_scalar() {
    use sx_plugin_api::stabby;
    let h = CalcHandler;
    let ctx = SxQueryContext::for_test();

    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received2 = Arc::clone(&received);
    let sender = SxResultSender::from_fn(move |r| {
        let value = r.metadata.iter().find(|e| e.key.as_str() == "value")
            .and_then(|e| e.value.clone().match_owned(|t| Some(t.into_string()), |_| None, |_| None));
        if let Some(s) = value {
            received2.lock().unwrap().push(s);
        }
        true
    });
    h.query(stabby::str::Str::from("2^32"), &ctx, sender);

    let got = received.lock().unwrap();
    assert_eq!(got.as_slice(), &["4294967296"]);
}
