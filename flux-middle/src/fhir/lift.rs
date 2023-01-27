///! "Lift" HIR types into  FHIR types.
use flux_common::{bug, iter::IterExt};
use flux_errors::ErrorGuaranteed;
use hir::{def::DefKind, def_id::DefId};
use itertools::Itertools;
use rustc_ast::LitKind;
use rustc_errors::IntoDiagnostic;
use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;

use crate::{early_ctxt::EarlyCtxt, fhir};

struct LiftCtxt<'a, 'sess, 'tcx> {
    early_cx: &'a EarlyCtxt<'sess, 'tcx>,
    def_id: LocalDefId,
}

pub fn lift_refined_by(early_cx: &EarlyCtxt, def_id: LocalDefId) -> fhir::RefinedBy {
    let item = early_cx.hir().expect_item(def_id);
    match item.kind {
        hir::ItemKind::TyAlias(..) | hir::ItemKind::Struct(..) | hir::ItemKind::Enum(..) => {
            fhir::RefinedBy {
                def_id,
                params: vec![],
                early_bound: 0,
                sorts: vec![],
                span: item.ident.span,
            }
        }
        _ => {
            bug!("expected struct, enum or type alias");
        }
    }
}

pub fn lift_alias(
    early_cx: &EarlyCtxt,
    def_id: LocalDefId,
) -> Result<fhir::Alias, ErrorGuaranteed> {
    let item = early_cx.tcx.hir().expect_item(def_id);
    let hir::ItemKind::TyAlias(ty, _) = &item.kind else {
        bug!("expected type alias");
    };
    let cx = LiftCtxt::new(early_cx, def_id);
    let ty = cx.lift_ty(ty)?;
    Ok(fhir::Alias { def_id, ty, span: item.span })
}

pub fn lift_fn_sig(
    early_cx: &EarlyCtxt,
    def_id: LocalDefId,
) -> Result<fhir::FnSig, ErrorGuaranteed> {
    let cx = LiftCtxt::new(early_cx, def_id);
    let hir_id = early_cx.hir().local_def_id_to_hir_id(def_id);
    let fn_decl = early_cx
        .hir()
        .fn_decl_by_hir_id(hir_id)
        .expect("item is does not have a `FnDecl`");

    let args = fn_decl
        .inputs
        .iter()
        .map(|ty| cx.lift_ty(ty))
        .try_collect_exhaust()?;

    let output = fhir::FnOutput {
        params: vec![],
        ensures: vec![],
        ret: cx.lift_fn_ret_ty(&fn_decl.output)?,
    };

    Ok(fhir::FnSig { params: vec![], requires: vec![], args, output })
}

impl<'a, 'sess, 'tcx> LiftCtxt<'a, 'sess, 'tcx> {
    fn new(early_cx: &'a EarlyCtxt<'sess, 'tcx>, def_id: LocalDefId) -> Self {
        Self { early_cx, def_id }
    }

    fn lift_fn_ret_ty(&self, ret_ty: &hir::FnRetTy) -> Result<fhir::Ty, ErrorGuaranteed> {
        match ret_ty {
            hir::FnRetTy::DefaultReturn(_) => Ok(fhir::Ty::Tuple(vec![])),
            hir::FnRetTy::Return(ty) => self.lift_ty(ty),
        }
    }

    fn lift_ty(&self, ty: &hir::Ty) -> Result<fhir::Ty, ErrorGuaranteed> {
        let ty = match &ty.kind {
            hir::TyKind::Slice(ty) => fhir::BaseTy::Slice(Box::new(self.lift_ty(ty)?)).into(),
            hir::TyKind::Array(ty, len) => {
                fhir::Ty::Array(Box::new(self.lift_ty(ty)?), self.lift_array_len(len)?)
            }
            hir::TyKind::Ref(_, mut_ty) => {
                fhir::Ty::Ref(lift_mutability(mut_ty.mutbl), Box::new(self.lift_ty(mut_ty.ty)?))
            }
            hir::TyKind::Never => fhir::Ty::Never,
            hir::TyKind::Tup(tys) => {
                fhir::Ty::Tuple(tys.iter().map(|ty| self.lift_ty(ty)).try_collect()?)
            }
            hir::TyKind::Path(hir::QPath::Resolved(_, path)) => self.lift_path(path)?,
            hir::TyKind::Ptr(mut_ty) => {
                fhir::Ty::RawPtr(Box::new(self.lift_ty(mut_ty.ty)?), mut_ty.mutbl)
            }
            _ => {
                return self.emit_unsupported(&format!(
                    "unsupported type: `{}`",
                    rustc_hir_pretty::ty_to_string(ty)
                ));
            }
        };
        Ok(ty)
    }

    fn lift_path(&self, path: &hir::Path) -> Result<fhir::Ty, ErrorGuaranteed> {
        let ty = match path.res {
            hir::def::Res::Def(DefKind::Struct | DefKind::Enum, def_id) => {
                let args = path.segments.last().unwrap().args;
                fhir::Ty::BaseTy(fhir::BaseTy::Adt(def_id, self.lift_generic_args(args)?))
            }
            hir::def::Res::Def(DefKind::TyAlias, def_id) => {
                let args = path.segments.last().unwrap().args;
                fhir::BaseTy::Alias(def_id, self.lift_generic_args(args)?, vec![]).into()
            }
            hir::def::Res::Def(DefKind::TyParam, def_id) => fhir::Ty::Param(def_id),
            hir::def::Res::PrimTy(hir::PrimTy::Bool) => fhir::BaseTy::Bool.into(),
            hir::def::Res::PrimTy(hir::PrimTy::Char) => fhir::Ty::Char,
            hir::def::Res::PrimTy(hir::PrimTy::Str) => fhir::Ty::Str,
            hir::def::Res::PrimTy(hir::PrimTy::Int(int_ty)) => fhir::BaseTy::Int(int_ty).into(),
            hir::def::Res::PrimTy(hir::PrimTy::Uint(uint_ty)) => fhir::BaseTy::Uint(uint_ty).into(),
            hir::def::Res::PrimTy(hir::PrimTy::Float(float_ty)) => fhir::Ty::Float(float_ty),
            hir::def::Res::SelfTyAlias { alias_to, .. } => self.lift_self_ty_alias(alias_to)?,
            _ => {
                return self.emit_unsupported(&format!(
                    "unsupported type: `{}`",
                    rustc_hir_pretty::path_to_string(path)
                ));
            }
        };
        Ok(ty)
    }

    fn lift_self_ty_alias(&self, alias_to: DefId) -> Result<fhir::Ty, ErrorGuaranteed> {
        let hir = self.early_cx.hir();
        let def_id = alias_to.expect_local();
        match hir.expect_item(def_id).kind {
            hir::ItemKind::Impl(parent_impl) => self.lift_ty(parent_impl.self_ty),
            _ => bug!("self types for structs and enums is not yet implemented"),
        }
    }

    fn lift_generic_args(
        &self,
        args: Option<&hir::GenericArgs>,
    ) -> Result<Vec<fhir::Ty>, ErrorGuaranteed> {
        let mut filtered = vec![];
        if let Some(args) = args {
            for arg in args.args {
                match arg {
                    hir::GenericArg::Lifetime(_) => {}
                    hir::GenericArg::Type(ty) => filtered.push(self.lift_ty(ty)?),
                    hir::GenericArg::Const(_) => {
                        return self.emit_unsupported("const generics are not supported")
                    }
                    hir::GenericArg::Infer(_) => {
                        bug!("unexpected inference generic argument");
                    }
                }
            }
        }
        Ok(filtered)
    }

    fn lift_array_len(&self, len: &hir::ArrayLen) -> Result<fhir::ArrayLen, ErrorGuaranteed> {
        let body = match len {
            hir::ArrayLen::Body(anon_const) => self.early_cx.hir().body(anon_const.body),
            hir::ArrayLen::Infer(_, _) => bug!("unexpected `ArrayLen::Infer`"),
        };
        if let hir::ExprKind::Lit(lit) = &body.value.kind
            && let LitKind::Int(array_len, _) = lit.node
        {
            Ok(fhir::ArrayLen {val: array_len as usize })
        } else {
            self.emit_unsupported("only interger literals are supported for array lengths")
        }
    }

    fn emit_unsupported<T>(&self, msg: &str) -> Result<T, ErrorGuaranteed> {
        self.emit_err(errors::UnsupportedHir::new(self.early_cx.tcx, self.def_id, msg))
    }

    fn emit_err<'b, T>(&'b self, err: impl IntoDiagnostic<'b>) -> Result<T, ErrorGuaranteed> {
        Err(self.early_cx.sess.emit_err(err))
    }
}

fn lift_mutability(mtbl: hir::Mutability) -> fhir::RefKind {
    match mtbl {
        hir::Mutability::Mut => fhir::RefKind::Mut,
        hir::Mutability::Not => fhir::RefKind::Shr,
    }
}

pub mod errors {
    use flux_macros::Diagnostic;
    use rustc_hir::def_id::DefId;
    use rustc_middle::ty::TyCtxt;
    use rustc_span::Span;

    #[derive(Diagnostic)]
    #[diag(lift::unsupported_hir, code = "FLUX")]
    #[note]
    pub struct UnsupportedHir<'a> {
        #[primary_span]
        #[label]
        span: Span,
        def_kind: &'static str,
        note: &'a str,
    }

    impl<'a> UnsupportedHir<'a> {
        pub fn new(tcx: TyCtxt, def_id: impl Into<DefId>, note: &'a str) -> Self {
            let def_id = def_id.into();
            let span = tcx
                .def_ident_span(def_id)
                .unwrap_or_else(|| tcx.def_span(def_id));
            let def_kind = tcx.def_kind(def_id).descr(def_id);
            Self { span, def_kind, note }
        }
    }
}