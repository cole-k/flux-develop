use crate::{
    constraint_builder::{ConstraintBuilder, Cursor},
    ty::{self, Expr},
    tyenv::{TyEnv, TyEnvBuilder},
};
use liquid_rust_common::index::IndexGen;
use liquid_rust_core::{ir, ty as core};
use rustc_hash::FxHashMap;

pub struct Subst {
    regions: FxHashMap<core::Name, ty::Region>,
    exprs: FxHashMap<core::Var, ty::Expr>,
    types: Vec<ty::Ty>,
}

pub enum InferenceError {
    PureParam(core::Ident),
    RegionParam(core::Ident),
}

pub fn lower_with_fresh_names(
    cursor: &mut Cursor,
    body: &ir::Body,
    fn_sig: &core::FnSig,
) -> (TyEnv, Vec<(ty::Region, ty::Ty)>, ty::Ty) {
    let mut subst = Subst::empty();
    let mut env_builder = TyEnvBuilder::new(body.nlocals);

    for param in &fn_sig.params {
        let fresh = cursor.fresh_var();
        subst.exprs.insert(
            core::Var::Free(param.name.name),
            ty::ExprKind::Var(fresh).intern(),
        );
        let expr = subst.lower_expr(&param.pred);
        cursor.push_forall(fresh, param.sort, ty::Pred::Expr(expr));
    }

    for (name, ty) in &fn_sig.requires {
        let ty = subst.lower_ty(cursor, ty);
        let ty = cursor.unpack(ty);
        let rvid = env_builder.define_abstract_region(ty);
        subst.regions.insert(name.name, ty::Region::from(rvid));
    }

    for (local, ty) in body.args_iter().zip(&fn_sig.args) {
        let ty = subst.lower_ty(cursor, ty);
        let ty = cursor.unpack(ty);
        env_builder.define_local(local, ty);
    }

    for local in body.vars_and_temps_iter() {
        env_builder.define_local(local, ty::TyKind::Uninit.intern());
    }

    env_builder.define_local(ir::RETURN_PLACE, ty::TyKind::Uninit.intern());

    let mut ensures = fn_sig
        .ensures
        .iter()
        .map(|(name, ty)| {
            let ty = subst.lower_ty(cursor, ty);
            (subst.regions[&name.name].clone(), ty)
        })
        .collect();

    let ret = subst.lower_ty(cursor, &fn_sig.ret);

    (env_builder.build(), ensures, ret)
}

impl Subst {
    pub fn new(cursor: &mut Cursor, types: &[core::Ty]) -> Self {
        let mut empty = Subst::empty();
        let types = types.iter().map(|ty| empty.lower_ty(cursor, ty)).collect();
        Self {
            exprs: FxHashMap::default(),
            regions: FxHashMap::default(),
            types,
        }
    }

    fn empty() -> Self {
        Self {
            exprs: FxHashMap::default(),
            regions: FxHashMap::default(),
            types: vec![],
        }
    }

    pub fn contains_key_for_expr(&self, var: core::Var) -> bool {
        self.exprs.contains_key(&var)
    }

    pub fn infer_from_fn_call(
        &mut self,
        env: &TyEnv,
        actuals: &[ty::Ty],
        fn_sig: &core::FnSig,
    ) -> Result<(), Vec<InferenceError>> {
        assert!(actuals.len() == fn_sig.args.len());

        for (actual, formal) in actuals.iter().zip(fn_sig.args.iter()) {
            self.infer_from_tys(actual.clone(), formal);
        }

        for (region, required) in &fn_sig.requires {
            let actual = env.lookup_region(self.lower_region(region.name)[0]);
            self.infer_from_tys(actual, required);
        }

        self.check_inference(fn_sig)
    }

    fn check_inference(&self, fn_sig: &core::FnSig) -> Result<(), Vec<InferenceError>> {
        let mut failed = vec![];
        for param in &fn_sig.params {
            if !self.exprs.contains_key(&core::Var::Free(param.name.name)) {
                failed.push(InferenceError::PureParam(param.name))
            }
        }

        for (region, _) in &fn_sig.requires {
            if !self.regions.contains_key(&region.name) {
                failed.push(InferenceError::RegionParam(*region))
            }
        }

        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed)
        }
    }

    fn infer_from_tys(&mut self, ty1: ty::Ty, ty2: &core::Ty) {
        match (ty1.kind(), ty2) {
            (
                ty::TyKind::Refine(bty1, e),
                core::Ty::Refine(
                    bty2,
                    core::Expr {
                        kind: core::ExprKind::Var(var, symbol, ..),
                        ..
                    },
                ),
            ) => {
                debug_assert!(bty1 == bty2);
                match self.exprs.insert(*var, e.clone()) {
                    Some(old_e) if &old_e != e => {
                        todo!("ambiguous instantiation for parameter `{}`", symbol)
                    }
                    _ => {}
                }
            }
            (ty::TyKind::MutRef(region1), core::Ty::MutRef(region2)) => {
                match self.regions.insert(region2.name, region1.clone()) {
                    Some(old_region) if &old_region != region1 => {
                        todo!(
                            "ambiguous instantiation for region parameter `{}`",
                            region2.symbol
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn lower_region(&self, name: core::Name) -> ty::Region {
        self.regions[&name].clone()
    }

    pub fn lower_ty(&mut self, cursor: &mut Cursor, ty: &core::Ty) -> ty::Ty {
        match ty {
            core::Ty::Refine(bty, e) => ty::TyKind::Refine(*bty, self.lower_expr(e)).intern(),
            core::Ty::Exists(bty, pred) => {
                let fresh = cursor.fresh_var();
                let pred = match pred {
                    core::Pred::Infer => cursor.fresh_kvar(fresh, bty.sort()),
                    core::Pred::Expr(e) => {
                        self.exprs
                            .insert(core::Var::Bound, ty::ExprKind::Var(fresh).intern());
                        let e = self.lower_expr(e);
                        self.exprs.remove(&core::Var::Bound);
                        ty::Pred::Expr(e)
                    }
                };
                ty::TyKind::Exists(*bty, fresh, pred).intern()
            }
            core::Ty::MutRef(region) => {
                ty::TyKind::MutRef(self.regions[&region.name].clone()).intern()
            }
            core::Ty::Param(param) => self
                .types
                .get(param.index as usize)
                .cloned()
                .unwrap_or_else(|| ty::TyKind::Param(*param).intern()),
        }
    }

    pub fn lower_expr(&self, expr: &core::Expr) -> ty::Expr {
        match &expr.kind {
            core::ExprKind::Var(var, _, _) => self.lower_var(*var),
            core::ExprKind::Literal(lit) => ty::ExprKind::Constant(self.lower_lit(*lit)).intern(),
            core::ExprKind::BinaryOp(op, e1, e2) => {
                ty::ExprKind::BinaryOp(lower_bin_op(*op), self.lower_expr(e1), self.lower_expr(e2))
                    .intern()
            }
        }
    }

    fn lower_var(&self, var: core::Var) -> ty::Expr {
        self.exprs.get(&var).unwrap().clone()
    }

    fn lower_lit(&self, lit: core::Lit) -> ty::Constant {
        match lit {
            core::Lit::Int(n) => ty::Constant::from(n),
            core::Lit::Bool(b) => ty::Constant::from(b),
        }
    }
}

fn lower_bin_op(op: core::BinOp) -> ty::BinOp {
    match op {
        core::BinOp::Eq => ty::BinOp::Eq,
        core::BinOp::Add => ty::BinOp::Add,
        core::BinOp::Gt => ty::BinOp::Gt,
        core::BinOp::Ge => ty::BinOp::Ge,
        core::BinOp::Lt => ty::BinOp::Lt,
        core::BinOp::Le => ty::BinOp::Le,
        core::BinOp::Or => ty::BinOp::Or,
        core::BinOp::And => ty::BinOp::And,
    }
}
