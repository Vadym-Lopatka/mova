//! P0b: core image. `p0b_image write <path>` / `p0b_image restore <path> [N]`
//! Both start from a natives-only boot (MOVA_BOOT_NATIVES_ONLY is set here).
use std::time::Instant;
use mova::internal::Interp;

fn natives() -> Interp {
    unsafe { std::env::set_var("MOVA_BOOT_NATIVES_ONLY", "1") };
    Interp::new()
}
fn hdr() -> String { mova::core_image::header() }

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(&a[2]);
    match a[1].as_str() {
        "write" => {
            let mut it = natives();
            let pre = mova::image::pre_index(&it);
            let src_base = 0usize;
            it.boot_rest_for_image();
            let t = Instant::now();
            let rep = mova::image::write_image(&it, &pre, &hdr(), src_base, &path).unwrap();
            println!("wrote {} bytes, {} objs in {:.1} ms; unsupported {:?}", rep.bytes, rep.objects, t.elapsed().as_secs_f64() * 1e3, rep.unsupported);
        }
        "restore" => {
            // single shot per process (source registry is process-global)
            let tn = Instant::now();
            let mut it = natives();
            let t_nat = tn.elapsed().as_secs_f64() * 1e3;
            let t = Instant::now();
            let ok = mova::image::restore(&mut it, &hdr(), &path).unwrap();
            let t_res = t.elapsed().as_secs_f64() * 1e3;
            assert!(ok, "restore returned false");
            it.image_post_restore();
            let t_all = t.elapsed().as_secs_f64() * 1e3;
            let t2 = Instant::now();
            let v = it.eval_str("t", "(+ 1 2)").unwrap();
            let t_eval = t2.elapsed().as_secs_f64() * 1e3;
            println!("natives_ms {:.3} restore_ms {:.3} restore+post_ms {:.3} first_eval_ms {:.3} result {}", t_nat, t_res, t_all, t_eval, mova::internal::pr_str(&v));
        }
        _ => {}
    }
}
