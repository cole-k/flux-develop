use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    slice,
};

use bitflags::bitflags;
use flux_common::index::{IndexGen, IndexVec};
use flux_fixpoint as fixpoint;
use flux_middle::rty::{
    box_args,
    evars::EVarSol,
    fold::{TypeFoldable, TypeVisitor},
    BaseTy, Expr, GenericArg, Name, RefKind, Sort, Ty, TyKind,
};
use itertools::Itertools;

use crate::{
    constraint_gen::Tag,
    fixpoint::{sort_to_fixpoint, FixpointCtxt, TagIdx},
};

/// A *refine*ment *tree* tracks the "tree-like structure" of refinement variables
/// and predicates generated during type-checking. The tree can then be converted into
/// a horn constraint which implies the safety of a program. Rather than constructing the
/// tree explicitly, it is constructed implicitly via the manipulation of [`RefineCtxt`].
pub struct RefineTree {
    root: NodePtr,
}

/// A *refine*ment *c*on*t*e*xt* tracks all the refinement parameters and predicates
/// available in a particular path during type-checking. For example, consider the following
/// program:
///
/// ```ignore
/// #[flux::sig(fn(i32[@a0], i32{v : v > a0})) -> i32]
/// fn add(x: i32, y: i32) -> i32 {
///     x + y
/// }
/// ```
///
/// At the beginning of the function, the refinement context will be `{a0: int, a1: int, a1 > a0}`,
/// where `a1` is a freshly generated name for the existential variable in the refinement of `y`.
pub struct RefineCtxt<'a> {
    _tree: &'a mut RefineTree,
    ptr: NodePtr,
}

/// A snapshot of a [`RefineCtxt`] at a particular point during type-checking. Snapshots
/// may become invalid when a refinement context is [`cleared`].
///
/// [`cleared`]: RefineTree::clear
pub struct Snapshot {
    ptr: WeakNodePtr,
}

/// A ist of refinement variables and their sorts.
#[derive(PartialEq, Eq)]
pub struct Scope {
    bindings: IndexVec<Name, Sort>,
}

struct Node {
    kind: NodeKind,
    /// Number of bindings between the root and this node's parent, i.e., we have
    /// as an invariant that `nbindings` equals the number of [`NodeKind::ForAll`]
    /// nodes from the parent of this node to the root.
    nbindings: usize,
    parent: Option<WeakNodePtr>,
    children: Vec<NodePtr>,
}

#[derive(Clone)]
struct NodePtr(Rc<RefCell<Node>>);
struct WeakNodePtr(Weak<RefCell<Node>>);

enum NodeKind {
    Conj,
    ForAll(Name, Sort),
    Guard(Expr),
    Head(Expr, Tag),
    Impl(Expr, Expr, Tag),
    True,
}

impl RefineTree {
    pub fn new() -> RefineTree {
        let root = Node { kind: NodeKind::Conj, nbindings: 0, parent: None, children: vec![] };
        let root = NodePtr(Rc::new(RefCell::new(root)));
        RefineTree { root }
    }

    pub fn refine_ctxt_at_root(&mut self) -> RefineCtxt {
        RefineCtxt { ptr: NodePtr(Rc::clone(&self.root)), _tree: self }
    }

    pub fn refine_ctxt_at(&mut self, snapshot: &Snapshot) -> Option<RefineCtxt> {
        Some(RefineCtxt { ptr: snapshot.ptr.upgrade()?, _tree: self })
    }

    #[allow(clippy::unused_self)]
    pub fn clear(&mut self, snapshot: &Snapshot) {
        if let Some(ptr) = snapshot.ptr.upgrade() {
            ptr.borrow_mut().children.clear();
        }
    }

    pub fn simplify(&mut self) {
        self.root.borrow_mut().simplify();
    }

    pub fn into_fixpoint(self, cx: &mut FixpointCtxt<Tag>) -> fixpoint::Constraint<TagIdx> {
        self.root
            .borrow()
            .to_fixpoint(cx)
            .unwrap_or(fixpoint::Constraint::TRUE)
    }
}

impl RefineCtxt<'_> {
    pub fn breadcrumb(&mut self) -> RefineCtxt {
        RefineCtxt { _tree: self._tree, ptr: NodePtr::clone(&self.ptr) }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { ptr: NodePtr::downgrade(&self.ptr) }
    }

    pub fn scope(&self) -> Scope {
        self.snapshot().scope().unwrap()
    }

    /// Defines a fresh refinement variable with the given `sort`. It returns the freshly
    /// generated name for the variable.
    pub fn define_var(&mut self, sort: &Sort) -> Name {
        self.ptr.push_foralls(slice::from_ref(sort)).pop().unwrap()
    }

    pub fn define_vars(&mut self, sorts: &[Sort]) -> Vec<Name> {
        self.ptr.push_foralls(sorts)
    }

    pub fn assume_pred(&mut self, pred: impl Into<Expr>) {
        self.ptr.push_guard(pred);
    }

    pub fn check_pred(&mut self, pred: impl Into<Expr>, tag: Tag) {
        let pred = pred.into();
        if !pred.is_trivially_true() {
            self.ptr.push_node(NodeKind::Head(pred, tag));
        }
    }

    pub fn check_impl(&mut self, pred1: impl Into<Expr>, pred2: impl Into<Expr>, tag: Tag) {
        self.ptr
            .push_node(NodeKind::Impl(pred1.into(), pred2.into(), tag));
    }

    fn unpack_bty(&mut self, bty: &BaseTy, inside_mut_ref: bool, flags: UnpackFlags) -> BaseTy {
        match bty {
            BaseTy::Adt(adt_def, substs) if adt_def.is_box() => {
                let (boxed, alloc) = box_args(substs);
                let boxed = if flags.contains(UnpackFlags::SHALLOW) {
                    boxed.clone()
                } else {
                    self.unpack_inner(boxed, inside_mut_ref, flags)
                };
                BaseTy::adt(
                    adt_def.clone(),
                    vec![GenericArg::Ty(boxed), GenericArg::Ty(alloc.clone())],
                )
            }
            _ => bty.clone(),
        }
    }

    fn unpack_inner(&mut self, ty: &Ty, in_mut_ref: bool, flags: UnpackFlags) -> Ty {
        match ty.kind() {
            TyKind::Indexed(bty, idxs) => {
                let bty = self.unpack_bty(bty, in_mut_ref, flags);
                Ty::indexed(bty, idxs.clone())
            }
            TyKind::Exists(exists) => {
                // HACK(nilehmann) In general we shouldn't unpack through mutable references because
                // that makes the refered type too specific. We only have this as a workaround to
                // infer parameters under mutable references and it should be removed once we implement
                // opening of mutable references. See also `ConstrGen::check_fn_call`.
                if !in_mut_ref || flags.contains(UnpackFlags::EXISTS_IN_MUT_REF) {
                    let exists =
                        exists.replace_bvars_with_fresh_fvars(|sort| self.define_var(sort));
                    self.ptr.push_guard(exists.pred);
                    let bty = self.unpack_bty(&exists.bty, in_mut_ref, flags);
                    Ty::indexed(bty, exists.args)
                } else {
                    ty.clone()
                }
            }
            TyKind::Constr(pred, ty) => {
                self.assume_pred(pred.clone());
                self.unpack_inner(ty, in_mut_ref, flags)
            }
            TyKind::Ref(rk, ty) => {
                let ty = if flags.contains(UnpackFlags::SHALLOW) {
                    ty.clone()
                } else {
                    self.unpack_inner(ty, matches!(rk, RefKind::Mut), flags)
                };
                Ty::mk_ref(*rk, ty)
            }
            TyKind::Tuple(tys) => {
                let tys = tys
                    .iter()
                    .map(|ty| self.unpack_inner(ty, in_mut_ref, flags))
                    .collect_vec();
                Ty::tuple(tys)
            }
            _ => ty.clone(),
        }
    }

    pub fn unpack_with(&mut self, ty: &Ty, flags: UnpackFlags) -> Ty {
        self.unpack_inner(ty, false, flags)
    }

    pub fn unpack(&mut self, ty: &Ty) -> Ty {
        self.unpack_inner(ty, false, UnpackFlags::empty())
    }

    pub fn assume_invariants(&mut self, ty: &Ty) {
        struct Visitor<'a, 'rcx>(&'a mut RefineCtxt<'rcx>);
        impl TypeVisitor for Visitor<'_, '_> {
            fn visit_bty(&mut self, bty: &BaseTy) {
                if let BaseTy::Adt(adt_def, substs) = bty && adt_def.is_box() {
                    substs.visit_with(self);
                }
            }

            fn visit_ty(&mut self, ty: &Ty) {
                if let TyKind::Indexed(bty, idxs) = ty.kind() {
                    for invariant in bty.invariants() {
                        let invariant = invariant.pred.replace_bvars(idxs.args());
                        self.0.assume_pred(invariant);
                    }
                }
                if !matches!(ty.kind(), TyKind::Exists(..)) {
                    ty.super_visit_with(self);
                }
            }
        }
        ty.visit_with(&mut Visitor(self))
    }

    pub fn replace_evars(&mut self, evars: &EVarSol) {
        self.ptr.borrow_mut().replace_evars(evars);
    }
}

impl Snapshot {
    /// Returns the [`scope`] at the snapshot if it is still valid or [`None`] otherwise.
    ///
    /// [`scope`]: Scope
    pub fn scope(&self) -> Option<Scope> {
        let parents = ParentsIter::new(self.ptr.upgrade()?);
        let bindings = parents
            .filter_map(|node| {
                let node = node.borrow();
                if let NodeKind::ForAll(_, sort) = &node.kind {
                    Some(sort.clone())
                } else {
                    None
                }
            })
            .collect_vec()
            .into_iter()
            .rev()
            .collect();
        Some(Scope { bindings })
    }
}

impl Scope {
    pub fn iter(&self) -> impl Iterator<Item = (Name, Sort)> + '_ {
        self.bindings
            .iter_enumerated()
            .map(|(name, sort)| (name, sort.clone()))
    }

    /// A generator of fresh names in this scope.
    pub fn name_gen(&self) -> IndexGen<Name> {
        IndexGen::skipping(self.bindings.len())
    }

    pub fn contains(&self, name: Name) -> bool {
        name.index() < self.bindings.len()
    }

    /// Whether `t` has any free variables not in this scope
    pub fn has_free_vars<T: TypeFoldable>(&self, t: &T) -> bool {
        !self.contains_all(t.fvars())
    }

    fn contains_all(&self, iter: impl IntoIterator<Item = Name>) -> bool {
        iter.into_iter().all(|name| self.contains(name))
    }
}

impl NodePtr {
    fn downgrade(this: &Self) -> WeakNodePtr {
        WeakNodePtr(Rc::downgrade(&this.0))
    }

    fn push_guard(&mut self, pred: impl Into<Expr>) {
        let pred = pred.into();
        if !pred.is_trivially_true() {
            *self = self.push_node(NodeKind::Guard(pred));
        }
    }

    fn push_foralls(&mut self, sorts: &[Sort]) -> Vec<Name> {
        let name_gen = self.name_gen();
        let mut names = vec![];
        for sort in sorts {
            let fresh = name_gen.fresh();
            names.push(fresh);
            *self = self.push_node(NodeKind::ForAll(fresh, sort.clone()));
        }
        names
    }

    fn name_gen(&self) -> IndexGen<Name> {
        IndexGen::skipping(self.next_name_idx())
    }

    fn push_node(&mut self, kind: NodeKind) -> NodePtr {
        debug_assert!(!matches!(self.borrow().kind, NodeKind::Head(..)));
        let node = Node {
            kind,
            nbindings: self.next_name_idx(),
            parent: Some(NodePtr::downgrade(self)),
            children: vec![],
        };
        let node = NodePtr(Rc::new(RefCell::new(node)));
        self.borrow_mut().children.push(NodePtr::clone(&node));
        node
    }

    fn next_name_idx(&self) -> usize {
        self.borrow().nbindings + usize::from(self.borrow().is_forall())
    }
}

bitflags! {
    pub struct UnpackFlags: u8 {
        const EXISTS_IN_MUT_REF = 0b01;
        const SHALLOW           = 0b10;
    }
}

impl WeakNodePtr {
    fn upgrade(&self) -> Option<NodePtr> {
        Some(NodePtr(self.0.upgrade()?))
    }
}

impl std::ops::Index<Name> for Scope {
    type Output = Sort;

    fn index(&self, name: Name) -> &Self::Output {
        &self.bindings[name]
    }
}

impl std::ops::Deref for NodePtr {
    type Target = Rc<RefCell<Node>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Node {
    fn simplify(&mut self) {
        for child in &self.children {
            child.borrow_mut().simplify();
        }

        match &self.kind {
            NodeKind::Head(pred, tag) => {
                let pred = pred.simplify();
                if pred.is_trivially_true() {
                    self.kind = NodeKind::True;
                } else {
                    self.kind = NodeKind::Head(pred, *tag);
                }
            }

            NodeKind::Impl(pred1, pred2, tag) => {
                let pred1 = pred1.simplify();
                let pred2 = pred2.simplify();
                if pred1 == pred2 || pred2.is_trivially_true() {
                    self.kind = NodeKind::True;
                } else {
                    self.kind = NodeKind::Impl(pred1, pred2, *tag);
                }
            }
            NodeKind::True => {}
            NodeKind::Guard(pred) => {
                self.children.drain_filter(|child| {
                    matches!(child.borrow().kind, NodeKind::True)
                        || matches!(&child.borrow().kind, NodeKind::Head(head, _) if head == pred)
                });
            }
            NodeKind::Conj | NodeKind::ForAll(..) => {
                self.children
                    .drain_filter(|child| matches!(&child.borrow().kind, NodeKind::True));
            }
        }
        if !self.is_leaf() && self.children.is_empty() {
            self.kind = NodeKind::True;
        }
    }

    fn is_leaf(&self) -> bool {
        matches!(self.kind, NodeKind::Head(..) | NodeKind::Impl(..) | NodeKind::True)
    }

    fn replace_evars(&mut self, sol: &EVarSol) {
        for child in &self.children {
            child.borrow_mut().replace_evars(sol);
        }
        match &mut self.kind {
            NodeKind::Guard(pred) => *pred = pred.replace_evars(sol),
            NodeKind::Impl(pred1, pred2, _) => {
                *pred1 = pred1.replace_evars(sol);
                *pred2 = pred2.replace_evars(sol);
            }
            NodeKind::Head(pred, _) => {
                *pred = pred.replace_evars(sol);
            }
            NodeKind::Conj | NodeKind::ForAll(..) | NodeKind::True => {}
        }
    }

    fn to_fixpoint(&self, cx: &mut FixpointCtxt<Tag>) -> Option<fixpoint::Constraint<TagIdx>> {
        match &self.kind {
            NodeKind::Conj | NodeKind::ForAll(_, Sort::Loc) => {
                children_to_fixpoint(cx, &self.children)
            }
            NodeKind::ForAll(name, sort) => {
                let fresh = cx.fresh_name();
                cx.with_name_map(*name, fresh, |cx| {
                    Some(fixpoint::Constraint::ForAll(
                        fresh,
                        sort_to_fixpoint(sort),
                        fixpoint::Pred::TRUE,
                        Box::new(children_to_fixpoint(cx, &self.children)?),
                    ))
                })
            }
            NodeKind::Guard(pred) => {
                let (bindings, pred) = cx.pred_to_fixpoint(pred);
                Some(stitch(
                    bindings,
                    fixpoint::Constraint::Guard(
                        pred,
                        Box::new(children_to_fixpoint(cx, &self.children)?),
                    ),
                ))
            }
            NodeKind::Impl(pred1, pred2, tag) => {
                let (bindings1, pred1) = cx.pred_to_fixpoint(pred1);
                let (bindings2, pred2) = cx.pred_to_fixpoint(pred2);
                Some(stitch(
                    bindings1,
                    fixpoint::Constraint::Guard(
                        pred1,
                        Box::new(stitch(
                            bindings2,
                            fixpoint::Constraint::Pred(pred2, Some(cx.tag_idx(*tag))),
                        )),
                    ),
                ))
            }
            NodeKind::Head(pred, tag) => {
                let (bindings, pred) = cx.pred_to_fixpoint(pred);
                Some(stitch(bindings, fixpoint::Constraint::Pred(pred, Some(cx.tag_idx(*tag)))))
            }
            NodeKind::True => None,
        }
    }

    /// Returns `true` if the node kind is [`ForAll`].
    ///
    /// [`ForAll`]: NodeKind::ForAll
    fn is_forall(&self) -> bool {
        matches!(self.kind, NodeKind::ForAll(..))
    }

    /// Returns `true` if the node kind is [`Head`].
    ///
    /// [`Head`]: NodeKind::Head
    fn is_head(&self) -> bool {
        matches!(self.kind, NodeKind::Head(..))
    }
}

fn children_to_fixpoint(
    cx: &mut FixpointCtxt<Tag>,
    children: &[NodePtr],
) -> Option<fixpoint::Constraint<TagIdx>> {
    let mut children = children
        .iter()
        .filter_map(|node| node.borrow().to_fixpoint(cx))
        .collect_vec();
    match children.len() {
        0 => None,
        1 => children.pop(),
        _ => Some(fixpoint::Constraint::Conj(children)),
    }
}

fn stitch(
    bindings: Vec<(fixpoint::Name, fixpoint::Sort, fixpoint::Expr)>,
    c: fixpoint::Constraint<TagIdx>,
) -> fixpoint::Constraint<TagIdx> {
    bindings.into_iter().rev().fold(c, |c, (name, sort, e)| {
        fixpoint::Constraint::ForAll(name, sort, fixpoint::Pred::Expr(e), Box::new(c))
    })
}

struct ParentsIter {
    ptr: Option<NodePtr>,
}

impl ParentsIter {
    fn new(ptr: NodePtr) -> Self {
        Self { ptr: Some(ptr) }
    }
}

impl Iterator for ParentsIter {
    type Item = NodePtr;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(ptr) = self.ptr.take() {
            self.ptr = ptr.borrow().parent.as_ref().and_then(WeakNodePtr::upgrade);
            Some(ptr)
        } else {
            None
        }
    }
}

mod pretty {
    use std::{
        fmt::{self, Write},
        slice,
    };

    use flux_common::format::PadAdapter;
    use flux_middle::pretty::*;
    use itertools::Itertools;

    use super::*;

    fn bindings_chain(ptr: &NodePtr) -> (Vec<(Name, Sort)>, Vec<NodePtr>) {
        fn go(ptr: &NodePtr, mut bindings: Vec<(Name, Sort)>) -> (Vec<(Name, Sort)>, Vec<NodePtr>) {
            let node = ptr.borrow();
            if let NodeKind::ForAll(name, sort) = &node.kind {
                bindings.push((*name, sort.clone()));
                if let [child] = &node.children[..] {
                    go(child, bindings)
                } else {
                    (bindings, node.children.clone())
                }
            } else {
                (bindings, vec![NodePtr::clone(ptr)])
            }
        }
        go(ptr, vec![])
    }

    fn flatten_conjs(nodes: &[NodePtr]) -> Vec<NodePtr> {
        fn go(ptr: &NodePtr, children: &mut Vec<NodePtr>) {
            let node = ptr.borrow();
            if let NodeKind::Conj = node.kind {
                for child in &node.children {
                    go(child, children);
                }
            } else {
                children.push(NodePtr::clone(ptr));
            }
        }
        let mut children = vec![];
        for ptr in nodes {
            go(ptr, &mut children);
        }
        children
    }

    fn preds_chain(ptr: &NodePtr) -> (Vec<Expr>, Vec<NodePtr>) {
        fn go(ptr: &NodePtr, mut preds: Vec<Expr>) -> (Vec<Expr>, Vec<NodePtr>) {
            let node = ptr.borrow();
            if let NodeKind::Guard(pred) = &node.kind {
                preds.push(pred.clone());
                if let [child] = &node.children[..] {
                    go(child, preds)
                } else {
                    (preds, node.children.clone())
                }
            } else {
                (preds, vec![NodePtr::clone(ptr)])
            }
        }
        go(ptr, vec![])
    }

    impl Pretty for RefineTree {
        fn fmt(&self, cx: &PPrintCx, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            define_scoped!(cx, f);
            w!("{:?}", &self.root)
        }
    }

    impl Pretty for NodePtr {
        fn fmt(&self, cx: &PPrintCx, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            define_scoped!(cx, f);
            let node = self.borrow();
            match &node.kind {
                NodeKind::Conj => {
                    let nodes = flatten_conjs(slice::from_ref(self));
                    w!("{:?}", join!("\n", nodes))
                }
                NodeKind::ForAll(name, sort) => {
                    let (bindings, children) = if cx.bindings_chain {
                        bindings_chain(self)
                    } else {
                        (vec![(*name, sort.clone())], node.children.clone())
                    };

                    w!(
                        "∀ {}.",
                        ^bindings
                            .into_iter()
                            .format_with(", ", |(name, sort), f| {
                                f(&format_args_cx!("{:?}: {:?}", ^name, sort))
                            })
                    )?;
                    fmt_children(&children, cx, f)
                }
                NodeKind::Guard(pred) => {
                    let (preds, children) = if cx.preds_chain {
                        preds_chain(self)
                    } else {
                        (vec![pred.clone()], node.children.clone())
                    };
                    let guard = Expr::and(preds).simplify();
                    w!("{:?} =>", parens!(guard, !guard.is_atom()))?;
                    fmt_children(&children, cx, f)
                }
                NodeKind::Head(pred, tag) => {
                    let pred = if cx.simplify_exprs { pred.simplify() } else { pred.clone() };
                    w!("{:?}", parens!(pred, !pred.is_atom()))?;
                    if cx.tags {
                        w!(" ~ {:?}", tag)?;
                    }
                    Ok(())
                }
                NodeKind::Impl(pred1, pred2, tag) => {
                    let pred1 = if cx.simplify_exprs { pred1.simplify() } else { pred1.clone() };
                    let pred2 = if cx.simplify_exprs { pred2.simplify() } else { pred2.clone() };
                    w!("{:?} => ", parens!(pred1, !pred1.is_atom()))?;
                    w!("{:?}", parens!(pred2, !pred2.is_atom()))?;
                    if cx.tags {
                        w!(" ~ {:?}", tag)?;
                    }
                    Ok(())
                }
                NodeKind::True => {
                    w!("true")
                }
            }
        }
    }

    fn fmt_children(
        children: &[NodePtr],
        cx: &PPrintCx,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let mut f = PadAdapter::wrap_fmt(f, 2);
        define_scoped!(cx, f);
        let children = flatten_conjs(children);
        match &children[..] {
            [] => w!(" true"),
            [n] => {
                if n.borrow().is_head() {
                    w!(" ")?;
                } else {
                    w!("\n")?;
                }
                w!("{:?}", NodePtr::clone(n))
            }
            _ => w!("\n{:?}", join!("\n", children)),
        }
    }

    impl Pretty for RefineCtxt<'_> {
        fn fmt(&self, cx: &PPrintCx, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            define_scoped!(cx, f);
            let parents = ParentsIter::new(NodePtr::clone(&self.ptr)).collect_vec();
            write!(
                f,
                "{{{}}}",
                parents
                    .into_iter()
                    .rev()
                    .filter(|ptr| {
                        let node = ptr.borrow();
                        match &node.kind {
                            NodeKind::ForAll(..) => true,
                            NodeKind::Guard(e) => !e.simplify().is_trivially_true(),
                            _ => false,
                        }
                    })
                    .format_with(", ", |n, f| {
                        let n = n.borrow();
                        match &n.kind {
                            NodeKind::ForAll(name, sort) => {
                                f(&format_args_cx!("{:?}: {:?}", ^name, sort))
                            }
                            NodeKind::Guard(pred) => f(&format_args_cx!("{:?}", pred)),
                            _ => unreachable!(),
                        }
                    })
            )
        }
    }

    impl Pretty for Scope {
        fn fmt(&self, cx: &PPrintCx, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            define_scoped!(cx, f);
            write!(
                f,
                "[{}]",
                self.bindings
                    .iter_enumerated()
                    .format_with(", ", |(name, sort), f| {
                        f(&format_args_cx!("{:?}: {:?}", ^name, sort))
                    })
            )
        }
    }

    impl_debug_with_default_cx!(RefineTree => "refine_tree", RefineCtxt<'_> => "refine_ctxt", Scope);
}