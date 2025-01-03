//! *Refining* is the process of generating a refined version of a rust type.
//!
//! Concretely, this module provides functions to go from types in [`flux_rustc_bridge::ty`] to
//! types in [`rty`].

use flux_arc_interner::List;
use flux_common::bug;
use flux_rustc_bridge::{ty, ty::GenericArgsExt as _};
use itertools::Itertools;
use rustc_hir::{def::DefKind, def_id::DefId};
use rustc_middle::ty::ParamTy;
use rustc_target::abi::VariantIdx;

use super::{fold::TypeFoldable, RefineArgsExt};
use crate::{
    global_env::GlobalEnv,
    queries::{QueryErr, QueryResult},
    query_bug, rty,
};

pub fn refine_generics(
    genv: GlobalEnv,
    def_id: DefId,
    generics: &ty::Generics,
) -> QueryResult<rty::Generics> {
    let is_box = if let DefKind::Struct = genv.def_kind(def_id) {
        genv.tcx().adt_def(def_id).is_box()
    } else {
        false
    };
    let params = generics
        .params
        .iter()
        .map(|param| {
            rty::GenericParamDef {
                kind: refine_generic_param_def_kind(is_box, param.kind),
                index: param.index,
                name: param.name,
                def_id: param.def_id,
            }
        })
        .collect();

    Ok(rty::Generics {
        own_params: params,
        parent: generics.parent(),
        parent_count: generics.parent_count(),
        has_self: generics.orig.has_self,
    })
}

pub fn refine_generic_param_def_kind(
    is_box: bool,
    kind: ty::GenericParamDefKind,
) -> rty::GenericParamDefKind {
    match kind {
        ty::GenericParamDefKind::Lifetime => rty::GenericParamDefKind::Lifetime,
        ty::GenericParamDefKind::Type { has_default } => {
            if is_box {
                rty::GenericParamDefKind::Type { has_default }
            } else {
                rty::GenericParamDefKind::Base { has_default }
            }
        }
        ty::GenericParamDefKind::Const { has_default, .. } => {
            rty::GenericParamDefKind::Const { has_default }
        }
    }
}

pub struct Refiner<'genv, 'tcx> {
    genv: GlobalEnv<'genv, 'tcx>,
    def_id: DefId,
    generics: rty::Generics,
    refine: fn(rty::BaseTy) -> rty::SubsetTyCtor,
}

impl<'genv, 'tcx> Refiner<'genv, 'tcx> {
    pub fn new_for_item(
        genv: GlobalEnv<'genv, 'tcx>,
        def_id: DefId,
        refine: fn(rty::BaseTy) -> rty::SubsetTyCtor,
    ) -> QueryResult<Self> {
        let generics = genv.generics_of(def_id)?;
        Ok(Self { genv, def_id, generics, refine })
    }

    pub fn default_for_item(genv: GlobalEnv<'genv, 'tcx>, def_id: DefId) -> QueryResult<Self> {
        Self::new_for_item(genv, def_id, refine_default)
    }

    pub fn with_holes(genv: GlobalEnv<'genv, 'tcx>, def_id: DefId) -> QueryResult<Self> {
        Self::new_for_item(genv, def_id, |bty| {
            let sort = bty.sort();
            let constr = rty::SubsetTy::new(
                bty.shift_in_escaping(1),
                rty::Expr::nu(),
                rty::Expr::hole(rty::HoleKind::Pred),
            );
            rty::Binder::bind_with_sort(constr, sort)
        })
    }

    pub(crate) fn refine_generic_predicates(
        &self,
        generics: &ty::GenericPredicates,
    ) -> QueryResult<rty::GenericPredicates> {
        Ok(rty::GenericPredicates {
            parent: generics.parent,
            predicates: self.refine_clauses(&generics.predicates)?,
        })
    }

    pub(crate) fn refine_clauses(&self, clauses: &[ty::Clause]) -> QueryResult<List<rty::Clause>> {
        let clauses = clauses
            .iter()
            .flat_map(|clause| self.refine_clause(clause).transpose())
            .try_collect()?;

        Ok(clauses)
    }

    fn refine_clause(&self, clause: &ty::Clause) -> QueryResult<Option<rty::Clause>> {
        let kind = match &clause.kind.as_ref().skip_binder() {
            ty::ClauseKind::Trait(trait_pred) => {
                let pred = rty::TraitPredicate {
                    trait_ref: self.refine_trait_ref(&trait_pred.trait_ref)?,
                };
                rty::ClauseKind::Trait(pred)
            }
            ty::ClauseKind::Projection(proj_pred) => {
                let rty::TyOrBase::Base(term) = self.refine_ty_or_base(&proj_pred.term)? else {
                    return Err(query_bug!(
                        self.def_id,
                        "sorry, we can't handle non-base associated types"
                    ));
                };
                let pred = rty::ProjectionPredicate {
                    projection_ty: self
                        .refine_alias_ty(ty::AliasKind::Projection, &proj_pred.projection_ty)?,
                    term,
                };
                rty::ClauseKind::Projection(pred)
            }
            ty::ClauseKind::TypeOutlives(pred) => {
                let pred = rty::OutlivesPredicate(self.refine_ty(&pred.0)?, pred.1);
                rty::ClauseKind::TypeOutlives(pred)
            }
            ty::ClauseKind::ConstArgHasType(const_, ty) => {
                rty::ClauseKind::ConstArgHasType(const_.clone(), self.as_default().refine_ty(ty)?)
            }
        };
        let kind = rty::Binder::bind_with_vars(kind, List::empty());
        Ok(Some(rty::Clause { kind }))
    }

    pub fn refine_existential_predicate(
        &self,
        poly_pred: &ty::PolyExistentialPredicate,
    ) -> QueryResult<rty::PolyExistentialPredicate> {
        self.refine_binders(poly_pred, |pred| {
            let pred = match pred {
                ty::ExistentialPredicate::Trait(trait_ref) => {
                    rty::ExistentialPredicate::Trait(rty::ExistentialTraitRef {
                        def_id: trait_ref.def_id,
                        args: self.refine_existential_predicate_generic_args(
                            trait_ref.def_id,
                            &trait_ref.args,
                        )?,
                    })
                }
                ty::ExistentialPredicate::Projection(projection) => {
                    let rty::TyOrBase::Base(term) = self.refine_ty_or_base(&projection.term)?
                    else {
                        return Err(query_bug!(
                            self.def_id,
                            "sorry, we can't handle non-base associated types"
                        ));
                    };
                    rty::ExistentialPredicate::Projection(rty::ExistentialProjection {
                        def_id: projection.def_id,
                        args: self.refine_existential_predicate_generic_args(
                            projection.def_id,
                            &projection.args,
                        )?,
                        term,
                    })
                }
                ty::ExistentialPredicate::AutoTrait(def_id) => {
                    rty::ExistentialPredicate::AutoTrait(*def_id)
                }
            };
            Ok(pred)
        })
    }

    pub fn refine_existential_predicate_generic_args(
        &self,
        def_id: DefId,
        args: &ty::GenericArgs,
    ) -> QueryResult<rty::GenericArgs> {
        let generics = self.generics_of(def_id)?;
        args.iter()
            .enumerate()
            .map(|(idx, arg)| {
                // We need to skip the generic for Self
                let param = generics.param_at(idx + 1, self.genv)?;
                self.refine_generic_arg(&param, arg)
            })
            .try_collect()
    }

    pub fn refine_trait_ref(&self, trait_ref: &ty::TraitRef) -> QueryResult<rty::TraitRef> {
        let trait_ref = rty::TraitRef {
            def_id: trait_ref.def_id,
            args: self.refine_generic_args(trait_ref.def_id, &trait_ref.args)?,
        };
        Ok(trait_ref)
    }

    pub fn refine_variant_def(
        &self,
        adt_def_id: DefId,
        variant_idx: VariantIdx,
    ) -> QueryResult<rty::PolyVariant> {
        let adt_def = self.adt_def(adt_def_id)?;
        let fields = adt_def
            .variant(variant_idx)
            .fields
            .iter()
            .map(|fld| {
                let ty = self.genv.lower_type_of(fld.did)?.instantiate_identity();
                self.refine_ty(&ty)
            })
            .try_collect()?;
        let value = rty::VariantSig::new(
            adt_def,
            rty::GenericArg::identity_for_item(self.genv, adt_def_id)?,
            fields,
            rty::Expr::unit_adt(adt_def_id),
        );
        Ok(rty::Binder::bind_with_vars(value, List::empty()))
    }

    pub fn refine_binders<S, T, F>(
        &self,
        t: &ty::Binder<S>,
        mut f: F,
    ) -> QueryResult<rty::Binder<T>>
    where
        F: FnMut(&S) -> QueryResult<T>,
    {
        let vars = refine_bound_variables(t.vars());
        let inner = t.as_ref().skip_binder();
        let inner = f(inner)?;
        Ok(rty::Binder::bind_with_vars(inner, vars))
    }

    pub fn refine_poly_fn_sig(&self, fn_sig: &ty::PolyFnSig) -> QueryResult<rty::PolyFnSig> {
        self.refine_binders(fn_sig, |fn_sig| {
            let inputs = fn_sig
                .inputs()
                .iter()
                .map(|ty| self.refine_ty(ty))
                .try_collect()?;
            let ret = self.refine_ty(fn_sig.output())?.shift_in_escaping(1);
            let output =
                rty::Binder::bind_with_vars(rty::FnOutput::new(ret, vec![]), List::empty());
            Ok(rty::FnSig::new(fn_sig.safety, fn_sig.abi, List::empty(), inputs, output))
        })
    }

    fn refine_generic_args(
        &self,
        def_id: DefId,
        args: &ty::GenericArgs,
    ) -> QueryResult<rty::GenericArgs> {
        let generics = self.generics_of(def_id)?;
        args.iter()
            .enumerate()
            .map(|(idx, arg)| {
                let param = generics.param_at(idx, self.genv)?;
                self.refine_generic_arg(&param, arg)
            })
            .collect()
    }

    pub fn refine_generic_arg(
        &self,
        param: &rty::GenericParamDef,
        arg: &ty::GenericArg,
    ) -> QueryResult<rty::GenericArg> {
        match (&param.kind, arg) {
            (rty::GenericParamDefKind::Type { .. }, ty::GenericArg::Ty(ty)) => {
                Ok(rty::GenericArg::Ty(self.refine_ty(ty)?))
            }
            (rty::GenericParamDefKind::Base { .. }, ty::GenericArg::Ty(ty)) => {
                let rty::TyOrBase::Base(contr) = self.refine_ty_or_base(ty)? else {
                    return Err(QueryErr::InvalidGenericArg { def_id: param.def_id });
                };
                Ok(rty::GenericArg::Base(contr))
            }
            (rty::GenericParamDefKind::Lifetime, ty::GenericArg::Lifetime(re)) => {
                Ok(rty::GenericArg::Lifetime(*re))
            }
            (rty::GenericParamDefKind::Const { .. }, ty::GenericArg::Const(ct)) => {
                Ok(rty::GenericArg::Const(ct.clone()))
            }
            _ => bug!("mismatched generic arg `{arg:?}` `{param:?}`"),
        }
    }

    fn refine_alias_ty(
        &self,
        alias_kind: ty::AliasKind,
        alias_ty: &ty::AliasTy,
    ) -> QueryResult<rty::AliasTy> {
        let def_id = alias_ty.def_id;
        let args = self.refine_generic_args(def_id, &alias_ty.args)?;

        let refine_args = if let ty::AliasKind::Opaque = alias_kind {
            rty::RefineArgs::for_item(self.genv, def_id, |param, _| {
                rty::Expr::hole(rty::HoleKind::Expr(param.sort.clone()))
            })?
        } else {
            List::empty()
        };

        Ok(rty::AliasTy::new(def_id, args, refine_args))
    }

    pub fn refine_ty(&self, ty: &ty::Ty) -> QueryResult<rty::Ty> {
        Ok(self.refine_ty_or_base(ty)?.into_ty())
    }

    pub fn refine_ty_or_base(&self, ty: &ty::Ty) -> QueryResult<rty::TyOrBase> {
        let bty = match ty.kind() {
            ty::TyKind::Closure(did, args) => {
                let closure_args = args.as_closure();
                let upvar_tys = closure_args
                    .upvar_tys()
                    .iter()
                    .map(|ty| self.refine_ty(ty))
                    .try_collect()?;
                rty::BaseTy::Closure(*did, upvar_tys, args.clone())
            }
            ty::TyKind::Coroutine(did, args) => {
                let args = args.as_coroutine();
                let resume_ty = self.refine_ty(args.resume_ty())?;
                let upvar_tys = args
                    .upvar_tys()
                    .map(|ty| self.refine_ty(ty))
                    .try_collect()?;
                rty::BaseTy::Coroutine(*did, resume_ty, upvar_tys)
            }
            ty::TyKind::CoroutineWitness(..) => {
                bug!("implement when we know what this is");
            }
            ty::TyKind::Never => rty::BaseTy::Never,
            ty::TyKind::Ref(r, ty, mutbl) => rty::BaseTy::Ref(*r, self.refine_ty(ty)?, *mutbl),
            ty::TyKind::Float(float_ty) => rty::BaseTy::Float(*float_ty),
            ty::TyKind::Tuple(tys) => {
                let tys = tys.iter().map(|ty| self.refine_ty(ty)).try_collect()?;
                rty::BaseTy::Tuple(tys)
            }
            ty::TyKind::Array(ty, len) => rty::BaseTy::Array(self.refine_ty(ty)?, len.clone()),
            ty::TyKind::Param(param_ty) => {
                match self.param(*param_ty)?.kind {
                    rty::GenericParamDefKind::Type { .. } => {
                        return Ok(rty::TyOrBase::Ty(rty::Ty::param(*param_ty)));
                    }
                    rty::GenericParamDefKind::Base { .. } => rty::BaseTy::Param(*param_ty),
                    rty::GenericParamDefKind::Lifetime | rty::GenericParamDefKind::Const { .. } => {
                        bug!()
                    }
                }
            }
            ty::TyKind::Adt(adt_def, args) => {
                let adt_def = self.genv.adt_def(adt_def.did())?;
                let args = self.refine_generic_args(adt_def.did(), args)?;
                rty::BaseTy::adt(adt_def, args)
            }
            ty::TyKind::FnDef(def_id, args) => {
                let args = self.refine_generic_args(*def_id, args)?;
                rty::BaseTy::fn_def(*def_id, args)
            }
            ty::TyKind::Alias(kind, alias_ty) => {
                let alias_ty = self.as_default().refine_alias_ty(*kind, alias_ty)?;
                rty::BaseTy::Alias(*kind, alias_ty)
            }
            ty::TyKind::Bool => rty::BaseTy::Bool,
            ty::TyKind::Int(int_ty) => rty::BaseTy::Int(*int_ty),
            ty::TyKind::Uint(uint_ty) => rty::BaseTy::Uint(*uint_ty),
            ty::TyKind::Str => rty::BaseTy::Str,
            ty::TyKind::Slice(ty) => rty::BaseTy::Slice(self.refine_ty(ty)?),
            ty::TyKind::Char => rty::BaseTy::Char,
            ty::TyKind::FnPtr(poly_fn_sig) => {
                rty::BaseTy::FnPtr(self.as_default().refine_poly_fn_sig(poly_fn_sig)?)
            }
            ty::TyKind::RawPtr(ty, mu) => {
                rty::BaseTy::RawPtr(self.as_default().refine_ty(ty)?, *mu)
            }
            ty::TyKind::Dynamic(exi_preds, r) => {
                let exi_preds = exi_preds
                    .iter()
                    .map(|ty| self.refine_existential_predicate(ty))
                    .try_collect()?;
                rty::BaseTy::Dynamic(exi_preds, *r)
            }
        };
        Ok(rty::TyOrBase::Base((self.refine)(bty)))
    }

    fn as_default(&self) -> Self {
        Refiner { refine: refine_default, generics: self.generics.clone(), ..*self }
    }

    fn adt_def(&self, def_id: DefId) -> QueryResult<rty::AdtDef> {
        self.genv.adt_def(def_id)
    }

    fn generics_of(&self, def_id: DefId) -> QueryResult<rty::Generics> {
        self.genv.generics_of(def_id)
    }

    fn param(&self, param_ty: ParamTy) -> QueryResult<rty::GenericParamDef> {
        self.generics.param_at(param_ty.index as usize, self.genv)
    }
}

fn refine_default(bty: rty::BaseTy) -> rty::SubsetTyCtor {
    let sort = bty.sort();
    let constr = rty::SubsetTy::trivial(bty.shift_in_escaping(1), rty::Expr::nu());
    rty::Binder::bind_with_sort(constr, sort)
}

pub fn refine_bound_variables(vars: &[ty::BoundVariableKind]) -> List<rty::BoundVariableKind> {
    vars.iter()
        .map(|kind| {
            match kind {
                ty::BoundVariableKind::Region(kind) => rty::BoundVariableKind::Region(*kind),
            }
        })
        .collect()
}
