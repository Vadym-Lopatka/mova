//! Zipper-like navigation over the CST for hover/completion: deepest node at a position, enclosing call, `ns :require` context.
use crate::cst::*;

pub struct Doc {
    pub cst: Cst,
    parent: Vec<u32>,
}

const NOPARENT: u32 = u32::MAX;

impl Doc {
    pub fn new(text: &str) -> Doc {
        let cst = crate::reader::parse(text);
        let mut parent = vec![NOPARENT; cst.len()];
        for i in 0..cst.len() {
            let id = NodeId(i as u32);
            for &c in cst.children(id) {
                parent[c.0 as usize] = id.0;
            }
        }
        Doc { cst, parent }
    }

    pub fn parent(&self, n: NodeId) -> Option<NodeId> {
        let p = self.parent[n.0 as usize];
        (p != NOPARENT).then_some(NodeId(p))
    }

    fn in_range(&self, n: NodeId, row: u32, col: u32) -> bool {
        let p = self.cst.pos(n);
        row >= p.row && row <= p.end_row && (row != p.row || col >= p.col) && (row != p.end_row || col < p.end_col)
    }

    fn inherits(&self, c: NodeId, parent: NodeId, row: u32, col: u32) -> bool {
        if self.in_range(c, row, col) {
            return true;
        }
        let cp = self.cst.pos(c);
        let pk = self.cst.kind(parent);
        // closing bracket: the cursor right after the last child of a collection selects that child
        cp.end_col == col
            && matches!(pk, Kind::List | Kind::Vector | Kind::Map | Kind::Set | Kind::AnonFn)
            && self.cst.children(parent).last() == Some(&c)
            && self.cst.node(c).end + 1 == self.cst.node(parent).end
            && self.in_range(parent, row, col)
    }

    /// rewrite-clj `find-at-pos`: deepest node containing (row, col); None when nothing contains it.
    pub fn find_at(&self, row: u32, col: u32) -> Option<NodeId> {
        let mut parent = self.cst.root();
        let mut cur = None;
        loop {
            let next = self.cst.children(parent).iter().copied().find(|&c| self.inherits(c, parent, row, col));
            match next {
                Some(c) => {
                    cur = Some(c);
                    if !Cst::is_container(self.cst.kind(c)) {
                        return cur;
                    }
                    parent = c;
                }
                None => return cur,
            }
        }
    }

    fn first_sig(&self, n: NodeId) -> Option<NodeId> {
        self.cst.sig_children(n).next()
    }

    /// `edit/find-function-usage-name-loc`: first child of the nearest enclosing list or `#()` (self included).
    pub fn func_name_node(&self, from: NodeId) -> Option<NodeId> {
        let mut n = Some(from);
        while let Some(x) = n {
            if matches!(self.cst.kind(x), Kind::List | Kind::AnonFn) {
                return self.first_sig(x);
            }
            n = self.parent(x);
        }
        None
    }

    fn leftmost(&self, n: NodeId) -> NodeId {
        match self.parent(n) {
            Some(p) => self.first_sig(p).unwrap_or(n),
            None => n,
        }
    }

    pub fn find_op(&self, n: NodeId) -> Option<NodeId> {
        let mut op = if self.cst.kind(n) == Kind::List { self.first_sig(n).unwrap_or_else(|| self.leftmost(n)) } else { self.leftmost(n) };
        loop {
            let up = self.parent(op)?;
            if self.cst.kind(up) == Kind::List {
                return Some(op);
            }
            op = self.leftmost(up);
        }
    }

    /// `edit/find-ops-up`.
    pub fn find_ops_up(&self, n: NodeId, ops: &[&str]) -> bool {
        let mut op = self.find_op(n);
        let mut guard = 0;
        while let Some(o) = op {
            guard += 1;
            if guard > 10_000 {
                return false;
            }
            if matches!(self.cst.kind(o), Kind::Symbol | Kind::Keyword) {
                let t = self.cst.text(o);
                let name = match t.rfind('/') {
                    Some(i) if i > 0 && i + 1 < t.len() => &t[i + 1..],
                    _ => t,
                };
                if ops.contains(&name) {
                    return true;
                }
            }
            op = self.parent(o).map(|u| self.leftmost(u));
            if let Some(x) = op {
                if self.parent(x).is_none() {
                    // reached the root's level: nothing above
                    let k = self.cst.kind(x);
                    if k == Kind::Root {
                        return false;
                    }
                }
            }
        }
        false
    }

    /// `edit/inside-require?`.
    pub fn inside_require(&self, n: NodeId) -> bool {
        (self.find_ops_up(n, &["ns"]) && self.find_ops_up(n, &[":require"])) || self.find_ops_up(n, &["require"])
    }
}
