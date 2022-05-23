use crate::{
    global_env::GlobalEnv,
    ty::{self, Path, VariantDef},
};
use itertools::Itertools;
use liquid_rust_common::index::{IndexGen, IndexVec};
use liquid_rust_core::ty as core;
use rustc_hash::FxHashMap;

pub struct LoweringCtxt<'a, 'tcx> {
    genv: &'a GlobalEnv<'tcx>,
    name_map: NameMap,
}

type NameMap = FxHashMap<core::Name, ty::Name>;

impl<'a, 'tcx> LoweringCtxt<'a, 'tcx> {
    pub fn empty(genv: &'a GlobalEnv<'tcx>) -> Self {
        Self { genv, name_map: FxHashMap::default() }
    }

    pub fn lower_fn_sig(genv: &GlobalEnv, fn_sig: core::FnSig) -> ty::Binders<ty::FnSig> {
        let name_gen = IndexGen::new();

        let mut cx = LoweringCtxt::empty(genv);

        let params = cx.lower_params(&name_gen, &fn_sig.params);

        let mut requires = vec![];
        for constr in fn_sig.requires {
            requires.push(cx.lower_constr(&constr));
        }

        let mut args = vec![];
        for ty in fn_sig.args {
            args.push(cx.lower_ty(&ty));
        }

        let mut ensures = vec![];
        for constr in fn_sig.ensures {
            ensures.push(cx.lower_constr(&constr));
        }

        let ret = cx.lower_ty(&fn_sig.ret);

        ty::Binders::new(params, ty::FnSig::new(requires, args, ret, ensures))
    }

    pub fn lower_adt_def(genv: &GlobalEnv, adt_def: &core::AdtDef) -> ty::AdtDef {
        let name_gen = IndexGen::new();
        let mut cx = LoweringCtxt::empty(genv);

        let refined_by = cx.lower_params(&name_gen, adt_def.refined_by());

        match &adt_def.kind {
            core::AdtDefKind::Transparent { variants, .. } => {
                let variants = variants
                    .iter()
                    .map(|variant| cx.lower_variant_def(variant))
                    .collect_vec();
                ty::AdtDef::transparent(adt_def.def_id, refined_by, IndexVec::from_raw(variants))
            }
            core::AdtDefKind::Opaque { .. } => ty::AdtDef::opaque(adt_def.def_id, refined_by),
        }
    }

    fn lower_variant_def(&self, variant_def: &core::VariantDef) -> VariantDef {
        let fields = variant_def
            .fields
            .iter()
            .map(|ty| self.lower_ty(ty))
            .collect_vec();
        VariantDef::new(fields)
    }

    fn lower_constr(&self, constr: &core::Constr) -> ty::Constr {
        match constr {
            core::Constr::Type(loc, ty) => {
                ty::Constr::Type(
                    Path::from(ty::Loc::Free(self.name_map[&loc.name])),
                    self.lower_ty(ty),
                )
            }
            core::Constr::Pred(e) => ty::Constr::Pred(lower_expr(e, &self.name_map)),
        }
    }

    fn lower_params(
        &mut self,
        name_gen: &IndexGen<ty::Name>,
        params: &[core::Param],
    ) -> Vec<ty::Param> {
        params
            .iter()
            .map(|param| {
                let fresh = name_gen.fresh();
                self.name_map.insert(param.name.name, fresh);
                ty::Param { name: fresh, sort: lower_sort(param.sort) }
            })
            .collect()
    }

    pub fn lower_qualifer(qualifier: &core::Qualifier) -> ty::Qualifier {
        let name_gen = IndexGen::new();
        let mut args = Vec::new();

        let mut name_map = NameMap::default();

        for param in &qualifier.args {
            let fresh = name_gen.fresh();
            name_map.insert(param.name.name, fresh);
            let sort = lower_sort(param.sort);
            args.push((fresh, sort));
        }

        let expr = lower_expr(&qualifier.expr, &name_map);

        ty::Qualifier { name: qualifier.name.clone(), args, expr }
    }

    pub fn lower_ty(&self, ty: &core::Ty) -> ty::Ty {
        match ty {
            core::Ty::Indexed(bty, refine) => {
                let exprs = refine
                    .exprs
                    .iter()
                    .map(|e| lower_expr(e, &self.name_map))
                    .collect_vec();
                ty::Ty::indexed(self.lower_base_ty(bty), exprs)
            }
            core::Ty::Exists(bty, pred) => {
                let bty = self.lower_base_ty(bty);
                let pred = match pred {
                    core::Pred::Hole => ty::Pred::Hole,
                    core::Pred::Expr(e) => ty::Pred::Expr(lower_expr(e, &self.name_map)),
                };
                ty::Ty::exists(bty, pred)
            }
            core::Ty::Ptr(loc) => ty::Ty::strg_ref(ty::Loc::Free(self.name_map[&loc.name])),
            core::Ty::Ref(rk, ty) => ty::Ty::mk_ref(Self::lower_ref_kind(*rk), self.lower_ty(ty)),
            core::Ty::Param(param) => ty::Ty::param(*param),
            core::Ty::Float(float_ty) => ty::Ty::float(*float_ty),
            core::Ty::Tuple(tys) => {
                let tys = tys.iter().map(|ty| self.lower_ty(ty)).collect_vec();
                ty::Ty::tuple(tys)
            }
            core::Ty::Never => ty::Ty::never(),
        }
    }

    fn lower_ref_kind(rk: core::RefKind) -> ty::RefKind {
        match rk {
            core::RefKind::Mut => ty::RefKind::Mut,
            core::RefKind::Shr => ty::RefKind::Shr,
        }
    }

    fn lower_base_ty(&self, bty: &core::BaseTy) -> ty::BaseTy {
        match bty {
            core::BaseTy::Int(int_ty) => ty::BaseTy::Int(*int_ty),
            core::BaseTy::Uint(uint_ty) => ty::BaseTy::Uint(*uint_ty),
            core::BaseTy::Bool => ty::BaseTy::Bool,
            core::BaseTy::Adt(did, substs) => {
                let adt_def = self.genv.adt_def(*did);
                let substs = substs.iter().map(|ty| self.lower_ty(ty));
                ty::BaseTy::adt(adt_def, substs)
            }
        }
    }
}

fn lower_expr(expr: &core::Expr, name_map: &NameMap) -> ty::Expr {
    match &expr.kind {
        core::ExprKind::Var(var, ..) => ty::Expr::var(lower_var(*var, name_map)),
        core::ExprKind::Literal(lit) => ty::Expr::constant(lower_lit(*lit)),
        core::ExprKind::BinaryOp(op, e1, e2) => {
            ty::Expr::binary_op(
                lower_bin_op(*op),
                lower_expr(e1, name_map),
                lower_expr(e2, name_map),
            )
        }
    }
}

fn lower_var(var: core::Var, name_map: &NameMap) -> ty::Var {
    match var {
        core::Var::Bound(idx) => ty::Var::Bound(idx),
        core::Var::Free(name) => ty::Var::Free(name_map[&name]),
    }
}

fn lower_lit(lit: core::Lit) -> ty::Constant {
    match lit {
        core::Lit::Int(n) => ty::Constant::from(n),
        core::Lit::Bool(b) => ty::Constant::from(b),
    }
}

pub fn lower_layout(layout: &core::Layout) -> ty::Layout {
    match layout {
        core::Layout::Bool => ty::Layout::bool(),
        core::Layout::Int(int_ty) => ty::Layout::int(*int_ty),
        core::Layout::Uint(uint_ty) => ty::Layout::uint(*uint_ty),
        core::Layout::Float(float_ty) => ty::Layout::float(*float_ty),
        core::Layout::Adt(def_id) => ty::Layout::adt(*def_id),
        core::Layout::Ref => ty::Layout::mk_ref(),
        core::Layout::Param => ty::Layout::param(),
        core::Layout::Tuple(layouts) => {
            ty::Layout::tuple(layouts.iter().map(lower_layout).collect_vec())
        }
        core::Layout::Never => ty::Layout::never(),
    }
}

pub fn lower_sort(sort: core::Sort) -> ty::Sort {
    match sort {
        core::Sort::Int => ty::Sort::int(),
        core::Sort::Bool => ty::Sort::bool(),
        core::Sort::Loc => ty::Sort::loc(),
    }
}

fn lower_bin_op(op: core::BinOp) -> ty::BinOp {
    match op {
        core::BinOp::Iff => ty::BinOp::Iff,
        core::BinOp::Imp => ty::BinOp::Imp,
        core::BinOp::Or => ty::BinOp::Or,
        core::BinOp::And => ty::BinOp::And,
        core::BinOp::Eq => ty::BinOp::Eq,
        core::BinOp::Gt => ty::BinOp::Gt,
        core::BinOp::Ge => ty::BinOp::Ge,
        core::BinOp::Lt => ty::BinOp::Lt,
        core::BinOp::Le => ty::BinOp::Le,
        core::BinOp::Add => ty::BinOp::Add,
        core::BinOp::Sub => ty::BinOp::Sub,
        core::BinOp::Mod => ty::BinOp::Mod,
        core::BinOp::Mul => ty::BinOp::Mul,
    }
}
