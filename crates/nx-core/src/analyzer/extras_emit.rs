//! Oracle-schema JSON of the extra buckets.
use super::emit::{bucket, obj, qname, W};
use super::expr::json_str;
use super::types::*;
use crate::cst::Pos;

fn num_or_null(w: &mut W, k: &str, v: u32) {
    if v == 0 {
        w.raw(k, "null");
    } else {
        w.num(k, v);
    }
}

fn name_pos_or_null(w: &mut W, p: Pos) {
    num_or_null(w, "name-row", p.row);
    num_or_null(w, "name-col", p.col);
    num_or_null(w, "name-end-row", p.end_row);
    num_or_null(w, "name-end-col", p.end_col);
}

pub(super) fn emit(out: &mut String, first: &mut bool, file: &str, fa: &FileAnalysis) {
    bucket(out, first, "keywords", &fa.keywords, |o, k| {
        obj(o, |w| {
            w.pos(k.pos);
            w.sym("name", k.name);
            w.sym("from", k.from);
            w.sym("from-var", k.from_var);
            // dependency analysis has no `:context` config (keys absent), project analysis `{}`
            if fa.has_callstack {
                w.raw("context", "{}");
            }
            w.sym("ns", k.ns);
            w.sym("alias", k.alias);
            w.sym("reg", k.reg);
            w.flag("auto-resolved", k.flags & KW_AUTO != 0);
            w.flag("namespace-from-prefix", k.flags & KW_PREFIX != 0);
            w.flag("keys-destructuring", k.flags & KW_KEYS_DESTR != 0);
            w.flag("keys-destructuring-ns-modifier", k.flags & KW_NS_MOD != 0);
            w.lang(k.lang);
        })
    });
    bucket(out, first, "symbols", &fa.symbols, |o, s| {
        obj(o, |w| {
            w.pos(s.pos);
            w.sym("name", s.name);
            w.sym("symbol", s.symbol);
            w.sym("to", s.to);
            w.sym("from", s.from);
            w.raw("context", "{}");
            w.raw("lang", ["\"clj\"", "\"clj\"", "\"cljs\"", "\"edn\""][s.lang as usize]);
        })
    });
    bucket(out, first, "protocol-impls", &fa.protocol_impls, |o, p| {
        obj(o, |w| {
            w.pos(p.pos);
            name_pos_or_null(w, p.name_pos);
            w.sym_or_null("method-name", p.method_name);
            w.sym_or_null("protocol-name", p.protocol_name);
            w.sym_or_null("protocol-ns", p.protocol_ns);
            w.sym_or_null("impl-ns", p.impl_ns);
            if p.defined_by.1.is_none() {
                w.raw("defined-by", "null");
                w.raw("defined-by->lint-as", "null");
            } else {
                w.str_("defined-by", &qname(p.defined_by));
                w.str_("defined-by->lint-as", &qname(p.defined_by_lint_as));
            }
            w.raw("derived-location", if p.derived { "true" } else { "null" });
        })
    });
    bucket(out, first, "instance-invocations", &fa.instance_invocations, |o, i| {
        obj(o, |w| {
            w.sym("method-name", i.method_name);
            w.name_pos(i.name_pos);
            w.flag("derived-location", i.derived);
            w.lang(i.lang);
        })
    });
    bucket(out, first, "java-class-usages", &fa.java_class_usages, |o, u| {
        obj(o, |w| {
            w.sym("class", u.class);
            w.sym("method-name", u.method);
            if u.flags & JU_CLJC != 0 {
                w.raw("uri", "null");
            } else {
                w.str_("uri", &format!("file:{}", file));
            }
            w.raw("call", ["null", "false", "true"][u.call as usize]);
            w.raw("lang", if u.flags & JU_CLJS != 0 { "\"cljs\"" } else { "\"clj\"" });
            w.pos(u.pos);
            if u.flags & JU_HAS_NAME != 0 {
                name_pos_or_null(w, u.name_pos);
            }
            w.flag("import", u.flags & JU_IMPORT != 0);
            w.flag("clj-kondo/mark-used", u.flags & JU_MARK_USED != 0);
            w.flag("skip-analysis", u.flags & JU_SKIP != 0);
            w.sym("branch", u.branch);
            if !u.tag.is_none() {
                w.raw("tag", &json_str(u.tag.as_str()));
                w.raw("user-meta", &format!("[{{\"tag\":{}}}]", json_str(u.tag.as_str())));
            }
        })
    });
    bucket(out, first, "java-class-definitions", &fa.java_class_defs, |o, d| {
        obj(o, |w| {
            w.sym("class", d.class);
            w.str_("uri", &format!("file:{}", file));
            w.key("flags");
            w.s.push('[');
            let mut f = true;
            for (n, b) in JF_NAMES {
                if d.flags & b != 0 {
                    if !f {
                        w.s.push(',');
                    }
                    f = false;
                    w.s.push_str(&json_str(n));
                }
            }
            w.s.push(']');
        })
    });
}
