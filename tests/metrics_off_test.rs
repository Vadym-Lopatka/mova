//! LSP_METRICS_SOCK unset: natives are no-ops returning nil, nothing is sent.
use mova::embed::Engine;

#[test]
fn disabled_is_noop() {
    std::env::remove_var("LSP_METRICS_SOCK");
    let mut e = Engine::builder().build();
    assert_eq!(e.eval_named("t", "(mova.metrics/enabled?)").unwrap().to_string(), "false");
    assert_eq!(e.eval_named("t", r#"(mova.metrics/span! "srv.x" 0 {:a 1})"#).unwrap().to_string(), "nil");
    assert_eq!(e.eval_named("t", "(mova.metrics/event! \"srv.y\" nil)").unwrap().to_string(), "nil");
    assert!(mova::metrics::now_ns() >= 0);
}
