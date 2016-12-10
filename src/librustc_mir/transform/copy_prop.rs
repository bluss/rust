// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Trivial copy propagation pass.
//!
//! This uses def-use analysis to remove values that have exactly one def and one use, which must
//! be an assignment.
//!
//! To give an example, we look for patterns that look like:
//!
//!     DEST = SRC
//!     ...
//!     USE(DEST)
//!
//! where `DEST` and `SRC` are both locals of some form. We replace that with:
//!
//!     NOP
//!     ...
//!     USE(SRC)
//!
//! The assignment `DEST = SRC` must be (a) the only mutation of `DEST` and (b) the only
//! (non-mutating) use of `SRC`. These restrictions are conservative and may be relaxed in the
//! future.

use def_use::DefUseAnalysis;
use rustc::mir::{Constant, Local, Location, Lvalue, Mir, Operand, Rvalue, StatementKind};
use rustc::mir::transform::{MirPass, MirSource, Pass};
use rustc::mir::visit::MutVisitor;
use rustc::mir::{BasicBlock, AggregateKind, Projection, ProjectionElem};
use rustc::ty::{Ty, TyCtxt};
use transform::qualify_consts;
use rustc_data_structures::indexed_vec::Idx;

pub struct CopyPropagation;

impl Pass for CopyPropagation {}

impl<'tcx> MirPass<'tcx> for CopyPropagation {
    fn run_pass<'a>(&mut self,
                    tcx: TyCtxt<'a, 'tcx, 'tcx>,
                    source: MirSource,
                    mir: &mut Mir<'tcx>) {
        match source {
            MirSource::Const(_) => {
                // Don't run on constants, because constant qualification might reject the
                // optimized IR.
                return
            }
            MirSource::Static(..) | MirSource::Promoted(..) => {
                // Don't run on statics and promoted statics, because trans might not be able to
                // evaluate the optimized IR.
                return
            }
            MirSource::Fn(function_node_id) => {
                if qualify_consts::is_const_fn(tcx, tcx.map.local_def_id(function_node_id)) {
                    // Don't run on const functions, as, again, trans might not be able to evaluate
                    // the optimized IR.
                    return
                }
            }
        }

        // We only run when the MIR optimization level is at least 1. This avoids messing up debug
        // info.
        // 
        // MIR optimization level 1 or higher is required to enable continued
        // iteration of copy propagation, as well as variant propagation.
        let optimize_more = match tcx.sess.opts.debugging_opts.mir_opt_level {
            Some(0) | None => return,
            Some(level) => level >= 1,
        };

        debug!("Running {}, optimize more={}", self.name(), optimize_more);

        loop {
            let mut def_use_analysis = DefUseAnalysis::new(mir);
            def_use_analysis.analyze(mir);

            let mut changed = false;
            for dest_local in mir.local_decls.indices() {
                debug!("Considering destination local: {:?}", dest_local);

                let action;
                let location;
                {
                    // The destination must have exactly one def.
                    let dest_use_info = def_use_analysis.local_info(dest_local);
                    let dest_def_count = dest_use_info.def_count_not_including_drop();
                    if dest_def_count == 0 {
                        debug!("  Can't copy-propagate local: dest {:?} undefined",
                               dest_local);
                        continue
                    }
                    if dest_def_count > 1 {
                        debug!("  Can't copy-propagate local: dest {:?} defined {} times",
                               dest_local,
                               dest_use_info.def_count());
                        continue
                    }
                    if dest_use_info.use_count() == 0 {
                        debug!("  Can't copy-propagate local: dest {:?} unused",
                               dest_local);
                        continue
                    }
                    let dest_lvalue_def = dest_use_info.defs_and_uses.iter().filter(|lvalue_def| {
                        lvalue_def.context.is_mutating_use() && !lvalue_def.context.is_drop()
                    }).next().unwrap();
                    location = dest_lvalue_def.location;

                    let basic_block = &mir[location.block];
                    let statement_index = location.statement_index;
                    let statement = match basic_block.statements.get(statement_index) {
                        Some(statement) => statement,
                        None => {
                            debug!("  Can't copy-propagate local: used in terminator");
                            continue
                        }
                    };

                    // That use of the source must be an assignment.
                    debug!(" statement.kind={:?}", statement.kind);
                    match statement.kind {
                        StatementKind::Assign(Lvalue::Local(local), Rvalue::Use(ref operand)) if
                                local == dest_local => {
                            let maybe_action = match *operand {
                                Operand::Consume(ref src_lvalue) => {
                                    Action::consume_lvalue(&mir,
                                                           tcx,
                                                           &def_use_analysis,
                                                           src_lvalue,
                                                           optimize_more)
                                }
                                Operand::Constant(ref src_constant) => {
                                    Action::constant(src_constant)
                                }
                            };
                            match maybe_action {
                                Some(this_action) => action = this_action,
                                None => continue,
                            }
                        }
                        _ => {
                            debug!("  Can't copy-propagate local: source use is not an \
                                    assignment");
                            continue
                        }
                    }
                }

                let new_change = action.perform(mir,
                                                tcx,
                                                &def_use_analysis,
                                                dest_local,
                                                location);
                changed |= new_change;
                if !optimize_more {
                    break;
                }
                // FIXME(pcwalton): Update the use-def chains to delete the instructions instead of
                // regenerating the chains.
                if new_change {
                    def_use_analysis.analyze(mir);
                }
            }
            if !changed {
                break
            }
        }
    }
}

enum Action<'tcx> {
    PropagateLocalCopy(Local),
    PropagateVariant(Local, Ty<'tcx>, usize /* variant */),
    PropagateConstant(Constant<'tcx>),
}

impl<'tcx> Action<'tcx> {
    fn consume_lvalue<'a>(mir: &Mir<'tcx>,
                          tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          def_use_analysis: &DefUseAnalysis,
                          src_lvalue: &Lvalue<'tcx>,
                          allow_variant_prop: bool)
                          -> Option<Action<'tcx>> {
        Action::local_copy(def_use_analysis, src_lvalue)
            .or_else(|| if allow_variant_prop {
                Action::variant(mir, tcx, def_use_analysis, src_lvalue)
            } else {
                None
            })
    }

    fn variant<'a>(mir: &Mir<'tcx>,
                   tcx: TyCtxt<'a, 'tcx, 'tcx>,
                   def_use_analysis: &DefUseAnalysis,
                   src_lvalue: &Lvalue<'tcx>)
                   -> Option<Action<'tcx>> {
        let lvalue_projection = if let Lvalue::Projection(ref proj) = *src_lvalue {
            &**proj
        } else {
            return None;
        };

        // Match:
        // X = ((SRC as Variant).Index: Type)
        // DST = Enum::<Type>::Variant(X)
        // with the restriction that the type of SRC is type of DST.
        // 
        // Replace with:
        // DST = SRC

        let src_local;
        let src_type;
        let src_adt_def;
        let src_variant;
        match *lvalue_projection {
           Projection {
                base: Lvalue::Projection(box Projection {
                    base: ref inner_base @ Lvalue::Local(_),
                    elem: ProjectionElem::Downcast(adt_def, variant),
                }),
                elem: ProjectionElem::Field(field_index, ..),
           } => {
               src_local = if let Lvalue::Local(local) = *inner_base {
                   local
               } else {
                   unreachable!();
               };
               if field_index.index() != 0 {
                   bug!("Variant has > 1 field");
               }
               src_type = inner_base.ty(mir, tcx).to_ty(tcx);
               src_adt_def = adt_def;
               src_variant = variant;
           }
           _ => {
               debug!(" Can't copy-propagate variant: not a variant");
               return None;
           }
        }
        let n_fields = src_adt_def.variants[src_variant].fields.len();
        if n_fields != 1 {
            debug!("  Can't copy-propagate variant: variant has {} fields (not 1)", n_fields);
            return None;
        }
        let src_use_info = def_use_analysis.local_info(src_local);
        let src_use_count = src_use_info.use_count();
        if src_use_count == 0 {
            debug!("  Can't copy-propagate variant: no uses");
            return None
        }
        Some(Action::PropagateVariant(src_local, src_type, src_variant))
    }

    fn local_copy(def_use_analysis: &DefUseAnalysis, src_lvalue: &Lvalue<'tcx>)
                  -> Option<Action<'tcx>> {
        // The source must be a local.
        let src_local = if let Lvalue::Local(local) = *src_lvalue {
            local
        } else {
            debug!("  Can't copy-propagate local: source is not a local");
            return None;
        };

        // We're trying to copy propagate a local.
        // There must be exactly one use of the source used in a statement (not in a terminator).
        let src_use_info = def_use_analysis.local_info(src_local);
        let src_use_count = src_use_info.use_count();
        if src_use_count == 0 {
            debug!("  Can't copy-propagate local: no uses");
            return None
        }
        if src_use_count != 1 {
            debug!("  Can't copy-propagate local: {} uses", src_use_info.use_count());
            return None
        }

        // Verify that the source doesn't change in between. This is done conservatively for now,
        // by ensuring that the source has exactly one mutation. The goal is to prevent things
        // like:
        //
        //     DEST = SRC;
        //     SRC = X;
        //     USE(DEST);
        //
        // From being misoptimized into:
        //
        //     SRC = X;
        //     USE(SRC);
        let src_def_count = src_use_info.def_count_not_including_drop();
        if src_def_count != 1 {
            debug!("  Can't copy-propagate local: {} defs of src",
                   src_use_info.def_count_not_including_drop());
            return None
        }

        Some(Action::PropagateLocalCopy(src_local))
    }

    fn constant(src_constant: &Constant<'tcx>) -> Option<Action<'tcx>> {
        Some(Action::PropagateConstant((*src_constant).clone()))
    }

    /// Return `true` if mir changed
    fn perform<'a>(self,
                   mir: &mut Mir<'tcx>,
                   tcx: TyCtxt<'a, 'tcx, 'tcx>,
                   def_use_analysis: &DefUseAnalysis<'tcx>,
                   dest_local: Local,
                   location: Location)
                   -> bool {
        match self {
            Action::PropagateLocalCopy(src_local) => {
                // Eliminate the destination and the assignment.
                //
                // First, remove all markers.
                //
                // FIXME(pcwalton): Don't do this. Merge live ranges instead.
                debug!("  Replacing all uses of {:?} with {:?} (local)",
                       dest_local,
                       src_local);
                for lvalue_use in &def_use_analysis.local_info(dest_local).defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }
                for lvalue_use in &def_use_analysis.local_info(src_local).defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }

                // Replace all uses of the destination local with the source local.
                let src_lvalue = Lvalue::Local(src_local);
                def_use_analysis.replace_all_defs_and_uses_with(dest_local, mir, src_lvalue);

                // Finally, zap the now-useless assignment instruction.
                debug!("  Deleting assignment");
                mir.make_statement_nop(location);

                true
            }
            Action::PropagateConstant(src_constant) => {
                // First, remove all markers.
                //
                // FIXME(pcwalton): Don't do this. Merge live ranges instead.
                debug!("  Replacing all uses of {:?} with {:?} (constant)",
                       dest_local,
                       src_constant);
                let dest_local_info = def_use_analysis.local_info(dest_local);
                for lvalue_use in &dest_local_info.defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }

                // Replace all uses of the destination local with the constant.
                let mut visitor = ConstantPropagationVisitor::new(dest_local,
                                                                  src_constant);
                for dest_lvalue_use in &dest_local_info.defs_and_uses {
                    visitor.visit_location(mir, dest_lvalue_use.location)
                }

                // Zap the assignment instruction if we eliminated all the uses. We won't have been
                // able to do that if the destination was used in a projection, because projections
                // must have lvalues on their LHS.
                let use_count = dest_local_info.use_count();
                if visitor.uses_replaced == use_count {
                    debug!("  {} of {} use(s) replaced; deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    mir.make_statement_nop(location);
                    true
                } else if visitor.uses_replaced == 0 {
                    debug!("  No uses replaced; not deleting assignment");
                    false
                } else {
                    debug!("  {} of {} use(s) replaced; not deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    true
                }
            }
            Action::PropagateVariant(src_local, src_type, variant) => {
                // we have DST = (SRC as Some).0: T
                // Find Some(DST: T) and replace with SRC.
                debug!("  Replacing variant constructions using {:?} with {:?}",
                       dest_local,
                       src_local);
                for lvalue_use in &def_use_analysis.local_info(dest_local).defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }

                let mut visitor = AssignmentVisitor::new(dest_local,
                                                         src_local,
                                                         src_type,
                                                         variant,
                                                         tcx);
                let dest_local_info = def_use_analysis.local_info(dest_local);
                for lvalue_use in &dest_local_info.defs_and_uses {
                    visitor.visit_location(mir, lvalue_use.location);
                }

                let use_count = dest_local_info.use_count();
                if visitor.uses_replaced == use_count {
                    debug!("  {} of {} use(s) replaced; deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    mir.make_statement_nop(location);
                    true
                } else if visitor.uses_replaced == 0 {
                    debug!("  No uses replaced; not deleting assignment");
                    false
                } else {
                    debug!("  {} of {} use(s) replaced; not deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    true
                }
            }
        }
    }
}

struct ConstantPropagationVisitor<'tcx> {
    dest_local: Local,
    constant: Constant<'tcx>,
    uses_replaced: usize,
}

impl<'tcx> ConstantPropagationVisitor<'tcx> {
    fn new(dest_local: Local, constant: Constant<'tcx>)
           -> ConstantPropagationVisitor<'tcx> {
        ConstantPropagationVisitor {
            dest_local: dest_local,
            constant: constant,
            uses_replaced: 0,
        }
    }
}

impl<'tcx> MutVisitor<'tcx> for ConstantPropagationVisitor<'tcx> {
    fn visit_operand(&mut self, operand: &mut Operand<'tcx>, location: Location) {
        self.super_operand(operand, location);

        match *operand {
            Operand::Consume(Lvalue::Local(local)) if local == self.dest_local => {}
            _ => return,
        }

        *operand = Operand::Constant(self.constant.clone());
        self.uses_replaced += 1
    }
}

struct AssignmentVisitor<'a, 'tcx: 'a> {
    replace_local: Local,
    src_local: Local,
    src_type: Ty<'tcx>,
    variant: usize,
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    uses_replaced: usize,
}

impl<'a, 'tcx> AssignmentVisitor<'a, 'tcx> {
    fn new(replace_local: Local,
           src_local: Local,
           src_type: Ty<'tcx>,
           variant: usize,
           tcx: TyCtxt<'a, 'tcx, 'tcx>) -> Self
    {
        Self {
            replace_local,
            src_local,
            src_type,
            variant,
            tcx,
            uses_replaced: 0,
        }
    }
}

impl<'a, 'tcx> MutVisitor<'tcx> for AssignmentVisitor<'a, 'tcx> {
    fn visit_assign(&mut self,
                    block: BasicBlock,
                    lvalue: &mut Lvalue<'tcx>,
                    rvalue: &mut Rvalue<'tcx>,
                    location: Location) {
        let mut replace = false;
        debug!("  Attempt to replace variant type={:?} variant={}", self.src_type, self.variant);
        match *rvalue {
            Rvalue::Aggregate(ref kind, ref operands) if operands.len() == 1 => {
                if let Operand::Consume(Lvalue::Local(local)) = operands[0] {
                    if local == self.replace_local {
                        if let AggregateKind::Adt(adt_def, variant, substs, None) = *kind {
                            let adt_type = self.tcx.mk_adt(adt_def, substs); 
                            if adt_type == self.src_type {
                                if self.variant == variant {
                                    // replace!
                                    replace = true;
                                }
                            } else {
                                debug!(" Can't propagate variant {:?}: not correct type",
                                       self.src_local);
                            }
                        }
                    }
                }
            }
            _ => { }
        }
        if replace {
            *rvalue = Rvalue::Use(Operand::Consume(Lvalue::Local(self.src_local)));
            self.uses_replaced += 1;
        } else {
            self.super_assign(block, lvalue, rvalue, location);
        }
    }
}
