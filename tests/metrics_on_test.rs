//! LSP_METRICS_SOCK set: datagrams arrive as JSON. Own binary: the env is read once per process.
use mova::embed::Engine;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

#[test]
fn span_event_and_runtime_sample_arrive() {
    let path = std::env::temp_dir().join(format!("mova-metrics-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let srv = UnixDatagram::bind(&path).unwrap();
    srv.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    std::env::set_var("LSP_METRICS_SOCK", &path);
    let mut e = Engine::builder().build();
    e.eval_named("t", r#"(mova.metrics/span! "srv.x" (- (mova.metrics/now-ns) 1000) {:a 1 :b "s" :c :kw :d nil :e/f 2.5 "g" true :h [1]})"#).unwrap();
    e.eval_named("t", r#"(mova.metrics/event! :srv.ev {:k 1})"#).unwrap();
    e.eval_named("t", "(dotimes [_ 50] (let [m {:a 1}] (assoc m :b 2) (assoc {:a 1} :c 3)))").unwrap();
    let mut buf = vec![0u8; 20000];
    let n = srv.recv(&mut buf).unwrap();
    let j: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
    assert_eq!(j["kind"], "span");
    assert_eq!(j["name"], "srv.x");
    assert!(j["dur_ns"].as_i64().unwrap() >= 1000);
    let a = &j["attrs"];
    assert_eq!((&a["a"], &a["b"], &a["c"], &a["e/f"], &a["g"]), (&1.into(), &"s".into(), &"kw".into(), &2.5.into(), &true.into()));
    assert!(a.get("d").is_none() && a.get("h").is_none());
    let n = srv.recv(&mut buf).unwrap();
    let j: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
    assert_eq!((j["kind"].as_str(), j["name"].as_str()), (Some("event"), Some("srv.ev")));
    // sampler: within ~4 s a mova.runtime sample with assoc counters
    let n = srv.recv(&mut buf).unwrap();
    let j: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
    assert_eq!(j["name"], "mova.runtime");
    let t = j["attrs"]["assoc.unique"].as_u64().unwrap() + j["attrs"]["assoc.shared"].as_u64().unwrap();
    assert!(t >= 50, "assoc total {t}");
    let _ = std::fs::remove_file(&path);
}
