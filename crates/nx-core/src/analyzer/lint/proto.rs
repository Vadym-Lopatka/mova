//! Protocol implementation linters: missing-protocol-method, unresolved-protocol-method,
//! protocol-method-arity-mismatch (kondo `linters/lint-protocol-impls!`).
use super::*;
use crate::analyzer::defs::{DefsIndex, FastMap};
use crate::analyzer::types::*;
use crate::intern::SymId;

#[derive(Clone, Debug)]
pub struct LMethod {
    pub name: SymId,
    pub pos: Pos,
    pub fixed: Arities,
    pub varargs: Option<u32>,
}

/// One registered protocol implementation (kondo `namespace/reg-protocol-impl!`).
#[derive(Clone, Debug)]
pub struct LProto {
    pub pos: Pos,
    pub pns: SymId,
    pub pname: SymId,
    pub methods: Vec<LMethod>,
    pub lang: u8,
}

/// Methods of a protocol definition: declaration order, allowed arities by name.
#[derive(Default, Clone, Debug)]
pub struct ProtoDef {
    pub methods: Vec<SymId>,
    pub arities: FastMap<SymId, Arities>,
}

impl<'a> Analyzer<'a> {
    pub fn lint_reg_proto(&mut self, node: NodeId, pns: SymId, pname: SymId, methods: Vec<LMethod>) {
        if !self.lon {
            return;
        }
        let pos = self.pos(node);
        self.out.lint_protos.push(LProto { pos, pns, pname, methods, lang: self.ltag });
    }
}

fn js(s: &str) -> String {
    crate::analyzer::expr::json_str(s)
}

/// Runs once all files are indexed.
pub fn check(fa: &mut FileAnalysis, defs: &DefsIndex) {
    if fa.lint_protos.is_empty() {
        return;
    }
    let lv = |t: FType| fa.lint_levels.get(t as usize).copied().unwrap_or_else(|| t.default_level());
    let (on_missing, on_unres, on_arity) = (lv(FType::MissingProtocolMethod) != OFF, lv(FType::UnresolvedProtocolMethod) != OFF, lv(FType::ProtocolMethodArityMismatch) != OFF);
    let protos = std::mem::take(&mut fa.lint_protos);
    for p in &protos {
        if p.pns.is_none() || p.pname.is_none() {
            continue;
        }
        let Some(def) = defs.protocol(crate::analyzer::defs::src_of(fa.base_lang.unwrap_or(BaseLang::Clj), p.lang), p.pns, p.pname) else { continue };
        let names: Vec<SymId> = p.methods.iter().map(|m| m.name).collect();
        if on_unres {
            for m in &p.methods {
                if !def.methods.contains(&m.name) {
                    let mut f = Finding::new(FType::UnresolvedProtocolMethod, m.pos, format!("Unresolved protocol method: {}", m.name.as_str()));
                    f.lang = p.lang;
                    fa.findings.push(f);
                }
            }
        }
        if on_missing {
            let missing: Vec<&str> = def.methods.iter().filter(|m| !names.contains(m)).map(|m| m.as_str()).collect();
            if !missing.is_empty() {
                let mut f = Finding::new(FType::MissingProtocolMethod, p.pos, format!("Missing protocol method(s): {}", missing.join(", ")));
                f.lang = p.lang;
                let ms: Vec<String> = p.methods.iter().map(|m| js(m.name.as_str())).collect();
                f.extra.push(("methods", format!("[{}]", ms.join(","))));
                f.extra.push(("protocol-name", js(p.pname.as_str())));
                f.extra.push(("protocol-ns", js(p.pns.as_str())));
                fa.findings.push(f);
            }
        }
        if on_arity {
            for m in &p.methods {
                let Some(&allowed) = def.arities.get(&m.name) else { continue };
                if m.varargs.is_some() {
                    continue;
                }
                for a in m.fixed.iter() {
                    if !allowed.has(a) {
                        let al: Vec<String> = allowed.iter().map(|x| x.to_string()).collect();
                        let mut f = Finding::new(FType::ProtocolMethodArityMismatch, m.pos, format!("Protocol method {} is implemented with arity {} but expects {}", m.name.as_str(), a, al.join(", ")));
                        f.lang = p.lang;
                        fa.findings.push(f);
                    }
                }
            }
        }
    }
    fa.lint_protos = protos;
}
