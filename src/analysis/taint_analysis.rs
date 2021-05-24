use rustc_index::bit_set::BitSet;
use rustc_middle::mir::{
    visit::Visitor, BasicBlock, Body, HasLocalDecls, Local, Location, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, TerminatorKind,
};

use rustc_mir::dataflow::{Analysis, AnalysisDomain, Forward};
use rustc_session::Session;
use rustc_span::Span;

use tracing::instrument;

use crate::eval::AttrInfo;

use super::taint_domain::TaintDomain;

/// A dataflow analysis that tracks whether a value may carry a taint.
///
/// Taints are introduced through sources, and consumed by sinks.
/// Ideally, a sink never consumes a tainted value - this should result in an error.
pub struct TaintAnalysis<'tcx, 'v> {
    session: &'tcx Session,
    info: &'v AttrInfo,
}

impl<'tcx, 'v> TaintAnalysis<'tcx, 'v> {
    pub fn new(session: &'tcx Session, info: &'v AttrInfo) -> Self {
        TaintAnalysis { session, info }
    }
}

impl<'tcx, 'v> AnalysisDomain<'tcx> for TaintAnalysis<'tcx, 'v> {
    type Domain = BitSet<Local>;
    const NAME: &'static str = "TaintAnalysis";

    type Direction = Forward;

    fn bottom_value(&self, body: &Body<'tcx>) -> Self::Domain {
        // bottom = untainted
        BitSet::new_empty(body.local_decls().len())
    }

    fn initialize_start_block(&self, _body: &Body<'tcx>, _state: &mut Self::Domain) {
        // Locals start out being untainted
    }
}

impl<'tcx, 'v> Analysis<'tcx> for TaintAnalysis<'tcx, 'v> {
    fn apply_statement_effect(
        &self,
        state: &mut Self::Domain,
        statement: &Statement<'tcx>,
        location: Location,
    ) {
        self.transfer_function(state, self.info)
            .visit_statement(statement, location);
    }

    fn apply_terminator_effect(
        &self,
        state: &mut Self::Domain,
        terminator: &Terminator<'tcx>,
        location: Location,
    ) {
        self.transfer_function(state, self.info)
            .visit_terminator(terminator, location);
    }

    fn apply_call_return_effect(
        &self,
        _state: &mut Self::Domain,
        _block: BasicBlock,
        _func: &Operand<'tcx>,
        _args: &[Operand<'tcx>],
        _return_place: Place<'tcx>,
    ) {
        // do nothing
    }
}

struct TransferFunction<'tcx, T> {
    state: &'tcx mut T,
    session: &'tcx Session,
    info: &'tcx AttrInfo,
}

impl<'tcx, 'v> TaintAnalysis<'tcx, 'v> {
    fn transfer_function<T>(
        &'tcx self,
        state: &'tcx mut T,
        info: &'v AttrInfo,
    ) -> TransferFunction<'tcx, T> {
        TransferFunction {
            state,
            session: self.session,
            info: self.info,
        }
    }
}

impl<'tcx, T: std::fmt::Debug> std::fmt::Debug for TransferFunction<'tcx, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:?}", &self.state))
    }
}

impl<'tcx, T: TaintDomain<Local> + std::fmt::Debug> Visitor<'tcx> for TransferFunction<'_, T> {
    fn visit_statement(&mut self, statement: &Statement<'tcx>, _: Location) {
        let Statement { source_info, kind } = statement;

        self.visit_source_info(source_info);

        // TODO(Hilmar): Match more statement kinds
        #[allow(clippy::single_match)]
        match kind {
            StatementKind::Assign(box (ref place, ref rvalue)) => {
                self.t_visit_assign(place, rvalue);
            }
            _ => (),
        }
    }

    fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, _: Location) {
        let Terminator { source_info, kind } = terminator;

        self.visit_source_info(source_info);

        match kind {
            TerminatorKind::Goto { .. } => {}
            TerminatorKind::SwitchInt { .. } => {}
            TerminatorKind::Return => {}
            TerminatorKind::Call {
                func,
                args,
                destination,
                fn_span,
                ..
            } => self.t_visit_call(func, args, destination, fn_span),
            TerminatorKind::Assert { .. } => {}
            _ => {}
        }
    }
}

impl<'tcx, T> TransferFunction<'tcx, T>
where
    Self: Visitor<'tcx>,
    T: TaintDomain<Local> + std::fmt::Debug,
{
    #[instrument]
    fn t_visit_assign(&mut self, place: &Place, rvalue: &Rvalue) {
        match rvalue {
            // If we assign a constant to a place, the place is clean.
            Rvalue::Use(Operand::Constant(_)) | Rvalue::UnaryOp(_, Operand::Constant(_)) => {
                self.state.mark_untainted(place.local)
            }

            // Otherwise we propagate the taint
            Rvalue::Use(Operand::Copy(f) | Operand::Move(f)) => {
                self.state.propagate(f.local, place.local);
            }

            Rvalue::BinaryOp(_, box b) | Rvalue::CheckedBinaryOp(_, box b) => match b {
                (Operand::Constant(_), Operand::Constant(_)) => {
                    self.state.mark_untainted(place.local);
                }
                (Operand::Copy(a) | Operand::Move(a), Operand::Copy(b) | Operand::Move(b)) => {
                    if self.state.is_tainted(a.local) || self.state.is_tainted(b.local) {
                        self.state.mark_tainted(place.local);
                    } else {
                        self.state.mark_untainted(place.local);
                    }
                }
                (Operand::Copy(p) | Operand::Move(p), Operand::Constant(_))
                | (Operand::Constant(_), Operand::Copy(p) | Operand::Move(p)) => {
                    self.state.propagate(p.local, place.local);
                }
            },
            Rvalue::UnaryOp(_, Operand::Move(p) | Operand::Copy(p)) => {
                self.state.propagate(p.local, place.local);
            }

            Rvalue::Repeat(_, _) => {}
            Rvalue::Ref(_, _, _) => {}
            Rvalue::ThreadLocalRef(_) => {}
            Rvalue::AddressOf(_, _) => {}
            Rvalue::Len(_) => {}
            Rvalue::Cast(_, _, _) => {}
            Rvalue::NullaryOp(_, _) => {}
            Rvalue::Discriminant(_) => {}
            Rvalue::Aggregate(_, _) => {}
        }
    }

    #[instrument]
    fn t_visit_call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &Option<(Place, BasicBlock)>,
        span: &Span,
    ) {
        let name = func
            .constant()
            .expect("Operand is not a function")
            .to_string();

        // TODO(Hilmar): Check if function is source, sink or sanitizer.
    }

    fn t_visit_source_destination(&mut self, destination: &Option<(Place, BasicBlock)>) {
        if let Some((place, _)) = destination {
            self.state.mark_tainted(place.local);
        }
    }

    fn t_visit_sink(&mut self, name: String, args: &[Operand], span: &Span) {
        if args.iter().map(|op| op.place()).any(|el| {
            if let Some(place) = el {
                self.state.is_tainted(place.local)
            } else {
                false
            }
        }) {
            self.session.emit_err(super::errors::TaintedSink {
                fn_name: name,
                span: *span,
            });
        }
    }
}
