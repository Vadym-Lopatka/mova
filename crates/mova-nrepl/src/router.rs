//! The router: decides what each request is and answers the native ops on
//! the IO thread, with no interpreter and no allocation on the usual path.
//!
//! Order of checks, as on the JVM (session middleware first):
//!
//! 1. `session` key present but unknown or closed: `unknown-session`
//!    (the session value is echoed as it came, whatever its type).
//! 2. native ops: `describe`, `clone`, `close`, `ls-sessions`.
//! 3. `eval` with no `code`: `no-code`; with an int `code`: `unknown-code-type`.
//! 4. the other ops (`eval`, `interrupt`, `stdin`, `load-file`, `completions`,
//!    `lookup`, `forward-system-output`) go to the `Backend`.
//! 5. anything else: `unknown-op`, echoing `op` (`[]` if there was none).
//!
//! A request with no `session` is ephemeral: every reply of it carries one
//! fresh session id that is not kept anywhere.

use crate::bencode::{write_reply, Kind, Request, V};
use crate::describe::write_describe;
use crate::outbox::Outbox;
use crate::reply::{status, Responder};
use crate::session::{Session, SessionId, SessionTable};
use crate::{Backend, Call};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

/// Ops that need the interpreter (or its per-session state).
const BACKEND_OPS: &[&[u8]] = &[
    b"eval",
    b"interrupt",
    b"stdin",
    b"load-file",
    b"completions",
    b"lookup",
    b"forward-system-output",
];

pub(crate) struct Router {
    pub sessions: Arc<SessionTable>,
    pub backend: Arc<dyn Backend>,
    pub verbose: bool,
    /// Middleware lane on: every request (after the session check) goes to
    /// `Backend::dispatch_slow`. One `bool` test per message; off by default.
    pub slow: bool,
}

impl Router {
    /// Handles one request. Native replies are appended to `out` (the
    /// connection's write buffer); backend ops get a `Responder` on `outbox`.
    pub(crate) fn route(&self, req: &Request<'_>, out: &mut Vec<u8>, outbox: &Outbox) {
        let id = req.id().map(|v| v.raw());

        // 1. session
        let session: Option<Arc<Session>> = match req.session() {
            Some(v) => match v.as_bytes().and_then(|b| self.sessions.get(b)) {
                Some(s) => Some(s),
                None => {
                    write_reply(out, id, V::Raw(v.raw()), &[("status", V::Strs(status::UNKNOWN_SESSION))]);
                    return;
                }
            },
            None => None,
        };
        // the id every reply of this request carries
        let sid = session.as_ref().map(|s| s.id).unwrap_or_else(SessionId::new);
        if self.slow {
            let reply = Responder::new(outbox.clone(), req.id(), sid);
            let call = Call { req, session, reply };
            let backend = &self.backend;
            if catch_unwind(AssertUnwindSafe(|| backend.dispatch_slow(call))).is_err() {
                eprintln!("nrepl: backend panicked in the middleware lane");
            }
            return;
        }
        self.route_resolved(req, session, sid, out, outbox);
    }

    /// The rest of `route`, once the session is resolved. The middleware lane
    /// calls it (through `NativeLane`) for what is left after the Mova handlers.
    pub(crate) fn route_resolved(
        &self,
        req: &Request<'_>,
        session: Option<Arc<Session>>,
        sid: SessionId,
        out: &mut Vec<u8>,
        outbox: &Outbox,
    ) {
        let id = req.id().map(|v| v.raw());
        let sess = V::Bytes(sid.as_bytes());

        let op = req.op();
        let opb = op.and_then(|v| v.as_bytes());
        match opb {
            // 2. native ops
            Some(b"describe") => {
                let versions = self.backend.versions();
                let verbose = req.verbose().is_some();
                match &session {
                    Some(s) => s.with_ns(|ns| write_describe(out, id, sess, ns, verbose, versions)),
                    None => write_describe(out, id, sess, "user", verbose, versions),
                }
            }
            Some(b"clone") => {
                let child = self.sessions.create();
                self.backend.session_cloned(session.as_ref(), &child);
                write_reply(
                    out,
                    id,
                    sess,
                    &[("new-session", V::Bytes(child.id.as_bytes())), ("status", V::Strs(status::DONE))],
                );
            }
            Some(b"close") => {
                if let Some(s) = &session {
                    if let Some(gone) = self.sessions.remove(s.id.as_bytes()) {
                        self.backend.session_closed(&gone);
                    }
                }
                write_reply(out, id, sess, &[("status", V::Strs(status::SESSION_CLOSED))]);
            }
            Some(b"ls-sessions") => {
                let ids = self.sessions.ids();
                let list: Vec<V<'_>> = ids.iter().map(|i| V::Bytes(i.as_bytes())).collect();
                write_reply(out, id, sess, &[("sessions", V::List(&list)), ("status", V::Strs(status::DONE))]);
            }
            // 3. eval argument checks
            Some(b"eval") if req.code().is_none() => {
                write_reply(out, id, sess, &[("status", V::Strs(status::NO_CODE))]);
            }
            Some(b"eval") if req.code().is_some_and(|c| c.kind() == Kind::Int) => {
                write_reply(out, id, sess, &[("status", V::Strs(status::UNKNOWN_CODE_TYPE))]);
            }
            // 4. backend
            Some(name) if BACKEND_OPS.contains(&name) => {
                let reply = Responder::new(outbox.clone(), req.id(), sid);
                let call = Call { req, session, reply };
                let backend = &self.backend;
                if catch_unwind(AssertUnwindSafe(|| backend.dispatch(call))).is_err() {
                    eprintln!("nrepl: backend panicked while handling op {}", String::from_utf8_lossy(name));
                }
            }
            // 5. unknown op: echo what was sent (`[]` when there was no op)
            _ => {
                let echo = V::Raw(op.map(|v| v.raw()).unwrap_or(b"le"));
                write_reply(out, id, sess, &[("op", echo), ("status", V::Strs(status::UNKNOWN_OP))]);
            }
        }
        if self.verbose {
            eprintln!("nrepl: op {}", opb.map(String::from_utf8_lossy).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{decode, parse_request, Parsed, Value};
    use crate::outbox::{ConnKey, Gate, Shared};
    use crate::poll::Poller;
    use std::sync::atomic::AtomicBool;

    struct Null;
    impl Backend for Null {
        fn dispatch(&self, call: Call<'_>) {
            call.reply.send_status(status::DONE);
        }
    }

    fn router() -> (Router, Outbox) {
        let p = Poller::new().unwrap();
        let shared = Arc::new(Shared::new(p.waker()));
        let ob = Outbox::new(shared, ConnKey(1), Arc::new(AtomicBool::new(true)), Arc::new(Gate::new()));
        (Router { sessions: Arc::new(SessionTable::new()), backend: Arc::new(Null), verbose: false, slow: false }, ob)
    }

    fn ask(r: &Router, ob: &Outbox, msg: &[u8]) -> Value {
        let Ok(Parsed::Message(req, _)) = parse_request(msg) else { panic!("bad request") };
        let mut out = Vec::new();
        r.route(&req, &mut out, ob);
        let (v, used) = decode(&out).unwrap().unwrap();
        assert_eq!(used, out.len(), "exactly one reply");
        v
    }

    fn statuses(v: &Value) -> Vec<String> {
        let Some(Value::List(l)) = v.get("status") else { panic!("no status in {v:?}") };
        l.iter().map(|x| x.as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn unknown_op_echoes_op_and_id() {
        let (r, ob) = router();
        let v = ask(&r, &ob, b"d2:id1:12:op6:nosuche");
        assert_eq!(statuses(&v), ["done", "unknown-op", "error"]);
        assert_eq!(v.get("op").unwrap().as_str(), Some("nosuch"));
        assert_eq!(v.get("id").unwrap().as_str(), Some("1"));
        // missing op is echoed as []
        let v = ask(&r, &ob, b"d2:id1:3e");
        assert_eq!(v.get("op"), Some(&Value::List(vec![])));
        // no id in, no id out
        assert!(ask(&r, &ob, b"d2:op1:xe").get("id").is_none());
    }

    #[test]
    fn ephemeral_session_id_is_fresh_per_message() {
        let (r, ob) = router();
        let a = ask(&r, &ob, b"d2:op1:xe");
        let b = ask(&r, &ob, b"d2:op1:xe");
        assert_ne!(a.get("session"), b.get("session"));
        assert!(r.sessions.is_empty());
    }

    #[test]
    fn clone_close_ls_sessions() {
        let (r, ob) = router();
        let c = ask(&r, &ob, b"d2:op5:clonee");
        let new = c.get("new-session").unwrap().as_str().unwrap().to_string();
        assert_ne!(c.get("session").unwrap().as_str().unwrap(), new);
        assert_eq!(statuses(&c), ["done"]);
        assert_eq!(r.sessions.len(), 1);

        // clone of a given session answers with that session
        let msg = format!("d2:op5:clone7:session{}:{}e", new.len(), new);
        let c2 = ask(&r, &ob, msg.as_bytes());
        assert_eq!(c2.get("session").unwrap().as_str(), Some(new.as_str()));
        assert_eq!(r.sessions.len(), 2);

        let l = ask(&r, &ob, b"d2:op11:ls-sessionse");
        let Some(Value::List(ids)) = l.get("sessions") else { panic!() };
        assert_eq!(ids.len(), 2);

        let msg = format!("d2:op5:close7:session{}:{}e", new.len(), new);
        let closed = ask(&r, &ob, msg.as_bytes());
        assert_eq!(statuses(&closed), ["done", "session-closed"]);
        assert_eq!(r.sessions.len(), 1);
        // closed session is unknown now, for any op
        let again = ask(&r, &ob, msg.as_bytes());
        assert_eq!(statuses(&again), ["done", "unknown-session", "error"]);
        assert_eq!(again.get("session").unwrap().as_str(), Some(new.as_str()));
        let msg = format!("d2:op8:describe7:session{}:{}e", new.len(), new);
        assert_eq!(statuses(&ask(&r, &ob, msg.as_bytes())), ["done", "unknown-session", "error"]);
    }

    #[test]
    fn close_without_session_still_says_closed() {
        let (r, ob) = router();
        assert_eq!(statuses(&ask(&r, &ob, b"d2:op5:closee")), ["done", "session-closed"]);
    }

    #[test]
    fn bad_session_values_are_echoed() {
        let (r, ob) = router();
        let v = ask(&r, &ob, b"d2:op8:describe7:session10:not-a-uuide");
        assert_eq!(statuses(&v), ["done", "unknown-session", "error"]);
        assert_eq!(v.get("session").unwrap().as_str(), Some("not-a-uuid"));
        let v = ask(&r, &ob, b"d2:op8:describe7:sessioni5ee");
        assert_eq!(v.get("session"), Some(&Value::Int(5)));
    }

    #[test]
    fn eval_argument_checks() {
        let (r, ob) = router();
        assert_eq!(statuses(&ask(&r, &ob, b"d2:op4:evale")), ["done", "no-code", "error"]);
        assert_eq!(statuses(&ask(&r, &ob, b"d4:codei5e2:op4:evale")), ["done", "unknown-code-type", "error"]);
    }

    #[test]
    fn describe_any_verbose_value_gives_docs() {
        let (r, ob) = router();
        let ops = |m: &[u8]| match ask(&r, &ob, m).get("ops") {
            Some(Value::Dict(d)) => d.get(&b"eval".to_vec()).cloned(),
            _ => panic!(),
        };
        assert_eq!(ops(b"d2:op8:describee"), Some(Value::dict([])));
        assert!(ops(b"d2:op8:describe8:verbose?5:falsee").unwrap().get("doc").is_some());
    }

    #[test]
    fn backend_ops_reach_the_backend() {
        let (r, ob) = router();
        // `Null` replies through the outbox, so the router itself writes nothing
        let Ok(Parsed::Message(req, _)) = parse_request(b"d4:code1:12:op4:evale") else { panic!() };
        let mut out = Vec::new();
        r.route(&req, &mut out, &ob);
        assert!(out.is_empty());
    }
}
