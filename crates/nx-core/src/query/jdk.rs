//! JDK interop answers: definition / hover for java class usages, resolved against the native JDK index.
use super::*;
use crate::jdk::{self, ClassRef, Jdk, MemberRef};

/// A resolved JDK class or member definition (kondo `java-class-definitions` / `java-member-definitions` element).
pub struct JdkHit {
    pub uri: String,
    /// `row col end-row end-col` (1-based, kondo); zeros for a class definition (no location in kondo).
    pub pos: Pos,
    /// Member name (None for a class definition).
    pub name: Option<String>,
    /// Return type of a method (not constructors / fields).
    pub ret: Option<String>,
    /// `(a, b)` of a method / constructor.
    pub params: Option<String>,
    pub doc: Option<String>,
}

impl JdkHit {
    pub fn bucket(&self) -> B {
        if self.name.is_some() { B::JavaMemberDef } else { B::JavaClassDef }
    }
    pub fn location(&self) -> String {
        format!("{{\"uri\":{},\"range\":{}}}", json_str(&self.uri), range_json(self.pos))
    }
}

fn member_hit(j: &Jdk, c: ClassRef, m: MemberRef, uri: String, with_doc: bool) -> JdkHit {
    let (row, col, end_row, end_col) = j.member_pos(m);
    let params = j.member_params(m).map(|p| format!("({})", p.join(", ")));
    JdkHit {
        uri,
        pos: Pos { row, col, end_row, end_col },
        name: Some(j.member_name(m).to_string()),
        ret: if j.is_method(m) { j.member_type(m).map(String::from) } else { None },
        params,
        doc: if with_doc { j.member_doc(c, m) } else { None },
    }
}

/// `find-definition` of a `:java-class-usages` element against the JDK: the member when the usage names one, else the class.
pub fn hit(q: &Q, e: El, with_doc: bool) -> Option<JdkHit> {
    if e.b != B::JavaClassUsage {
        return None;
    }
    let u = &q.fa(e.f).java_class_usages[e.i as usize];
    // cljc: the JVM golden (cljs project, `System/currentTimeMillis` in a :clj branch) resolves no JDK definition
    if u.flags & JU_CLJS != 0 || u.class.is_none() || q.uri(e.f).ends_with(".cljc") {
        return None;
    }
    let j = jdk::wait(jdk::QUERY_WAIT)?;
    let c = j.class(u.class.as_str())?;
    // JVM shape: `file://` of the extracted source; fall back to the zip entry when extraction fails
    let uri = match jdk::extracted_path(&j, c) {
        Some(p) => crate::engine::scan::path_to_uri(&p),
        None => jdk::entry_uri(&j, j.class_entry(c), q.s.opts.jar_scheme),
    };
    if !u.method.is_none() {
        if let Some(m) = j.find_member(c, u.method.as_str()) {
            return Some(member_hit(&j, c, m, uri, with_doc));
        }
    }
    Some(JdkHit { uri, pos: Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, name: None, ret: None, params: None, doc: None })
}

/// `find-definition` of a `:java-class-usages` element against the classpath jars' `.class` files: the decompiled
/// `.java` location `{uri, range 0:0-0:0}` (kondo gives class definitions no position).
pub fn jar_class_location(q: &Q, e: El) -> Option<String> {
    if e.b != B::JavaClassUsage {
        return None;
    }
    let u = &q.fa(e.f).java_class_usages[e.i as usize];
    if u.flags & JU_CLJS != 0 || u.class.is_none() {
        return None;
    }
    let (jar, entry) = q.s.jars.as_ref()?.layer.find_class(u.class.as_str())?;
    let root = &q.s.project.as_ref()?.root;
    let uri = super::decompile::decompiled_uri(root, jar, entry, q.s.opts.jar_scheme);
    Some(format!("{{\"uri\":{},\"range\":{}}}", json_str(&uri), range_json(Pos { row: 0, col: 0, end_row: 0, end_col: 0 })))
}

/// Does `find-definition` of this `:java-class-usages` element find a class (JDK source or classpath jar)?
pub fn has_class_definition(q: &Q, e: El) -> bool {
    if e.b != B::JavaClassUsage {
        return false;
    }
    if hit(q, e, false).is_some() {
        return true;
    }
    let u = &q.fa(e.f).java_class_usages[e.i as usize];
    u.flags & JU_CLJS == 0 && !u.class.is_none() && q.s.jars.as_ref().is_some_and(|j| j.layer.find_class(u.class.as_str()).is_some())
}

/// Hover target of a java class usage: the JDK source, else the `.class` entry of a dependency jar (JVM: `jar:..!/x.class`).
pub fn hover_hit(q: &Q, e: El) -> Option<JdkHit> {
    if let Some(h) = hit(q, e, true) {
        return Some(h);
    }
    if e.b != B::JavaClassUsage {
        return None;
    }
    let u = &q.fa(e.f).java_class_usages[e.i as usize];
    if u.flags & JU_CLJS != 0 || u.class.is_none() {
        return None;
    }
    let (jar, entry) = q.s.jars.as_ref()?.layer.find_class(u.class.as_str())?;
    let uri = crate::engine::jarview::jar_entry_uri(jar, entry, q.s.opts.jar_scheme);
    Some(JdkHit { uri, pos: Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, name: None, ret: None, params: None, doc: None })
}
