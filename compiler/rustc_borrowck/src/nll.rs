//! The entry point of the NLL borrow checker.

use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::{env, io};

use polonius_engine::{Algorithm, Output};
use rustc_data_structures::fx::FxIndexMap;
use rustc_hir::def_id::LocalDefId;
use rustc_index::IndexSlice;
use rustc_middle::mir::pretty::{PrettyPrintMirOptions, dump_mir_with_options};
use rustc_middle::mir::{
    Body, ClosureOutlivesSubject, ClosureRegionRequirements, PassWhere, Promoted, create_dump_file,
    dump_enabled, dump_mir,
};
use rustc_middle::ty::print::with_no_trimmed_paths;
use rustc_middle::ty::{self, OpaqueHiddenType, TyCtxt};
use rustc_mir_dataflow::ResultsCursor;
use rustc_mir_dataflow::impls::MaybeInitializedPlaces;
use rustc_mir_dataflow::move_paths::MoveData;
use rustc_mir_dataflow::points::DenseLocationMap;
use rustc_session::config::MirIncludeSpans;
use rustc_span::sym;
use tracing::{debug, instrument};

use crate::borrow_set::BorrowSet;
use crate::consumers::ConsumerOptions;
use crate::diagnostics::RegionErrors;
use crate::facts::{AllFacts, AllFactsExt, RustcFacts};
use crate::location::LocationTable;
use crate::polonius::LocalizedOutlivesConstraintSet;
use crate::region_infer::RegionInferenceContext;
use crate::type_check::{self, MirTypeckResults};
use crate::universal_regions::UniversalRegions;
use crate::{BorrowckInferCtxt, polonius, renumber};

pub type PoloniusOutput = Output<RustcFacts>;

/// The output of `nll::compute_regions`. This includes the computed `RegionInferenceContext`, any
/// closure requirements to propagate, and any generated errors.
pub(crate) struct NllOutput<'tcx> {
    pub regioncx: RegionInferenceContext<'tcx>,
    pub opaque_type_values: FxIndexMap<LocalDefId, OpaqueHiddenType<'tcx>>,
    pub polonius_input: Option<Box<AllFacts>>,
    pub polonius_output: Option<Box<PoloniusOutput>>,
    pub opt_closure_req: Option<ClosureRegionRequirements<'tcx>>,
    pub nll_errors: RegionErrors<'tcx>,

    /// When using `-Zpolonius=next`: the localized typeck and liveness constraints.
    pub localized_outlives_constraints: Option<LocalizedOutlivesConstraintSet>,
}

/// Rewrites the regions in the MIR to use NLL variables, also scraping out the set of universal
/// regions (e.g., region parameters) declared on the function. That set will need to be given to
/// `compute_regions`.
#[instrument(skip(infcx, body, promoted), level = "debug")]
pub(crate) fn replace_regions_in_mir<'tcx>(
    infcx: &BorrowckInferCtxt<'tcx>,
    body: &mut Body<'tcx>,
    promoted: &mut IndexSlice<Promoted, Body<'tcx>>,
) -> UniversalRegions<'tcx> {
    let def = body.source.def_id().expect_local();

    debug!(?def);

    // Compute named region information. This also renumbers the inputs/outputs.
    let universal_regions = UniversalRegions::new(infcx, def);

    // Replace all remaining regions with fresh inference variables.
    renumber::renumber_mir(infcx, body, promoted);

    dump_mir(infcx.tcx, false, "renumber", &0, body, |_, _| Ok(()));

    universal_regions
}

/// Computes the (non-lexical) regions from the input MIR.
///
/// This may result in errors being reported.
pub(crate) fn compute_regions<'a, 'tcx>(
    infcx: &BorrowckInferCtxt<'tcx>,
    universal_regions: UniversalRegions<'tcx>,
    body: &Body<'tcx>,
    promoted: &IndexSlice<Promoted, Body<'tcx>>,
    location_table: &LocationTable,
    flow_inits: ResultsCursor<'a, 'tcx, MaybeInitializedPlaces<'a, 'tcx>>,
    move_data: &MoveData<'tcx>,
    borrow_set: &BorrowSet<'tcx>,
    consumer_options: Option<ConsumerOptions>,
) -> NllOutput<'tcx> {
    let is_polonius_legacy_enabled = infcx.tcx.sess.opts.unstable_opts.polonius.is_legacy_enabled();
    let polonius_input = consumer_options.map(|c| c.polonius_input()).unwrap_or_default()
        || is_polonius_legacy_enabled;
    let polonius_output = consumer_options.map(|c| c.polonius_output()).unwrap_or_default()
        || is_polonius_legacy_enabled;
    let mut all_facts =
        (polonius_input || AllFacts::enabled(infcx.tcx)).then_some(AllFacts::default());

    let elements = Rc::new(DenseLocationMap::new(body));

    // Run the MIR type-checker.
    let MirTypeckResults {
        constraints,
        universal_region_relations,
        opaque_type_values,
        mut polonius_context,
    } = type_check::type_check(
        infcx,
        body,
        promoted,
        universal_regions,
        location_table,
        borrow_set,
        &mut all_facts,
        flow_inits,
        move_data,
        Rc::clone(&elements),
    );

    // Create the region inference context, taking ownership of the
    // region inference data that was contained in `infcx`, and the
    // base constraints generated by the type-check.
    let var_infos = infcx.get_region_var_infos();

    // If requested, emit legacy polonius facts.
    polonius::legacy::emit_facts(
        &mut all_facts,
        infcx.tcx,
        location_table,
        body,
        borrow_set,
        move_data,
        &universal_region_relations,
        &constraints,
    );

    let mut regioncx = RegionInferenceContext::new(
        infcx,
        var_infos,
        constraints,
        universal_region_relations,
        elements,
    );

    // If requested for `-Zpolonius=next`, convert NLL constraints to localized outlives
    // constraints.
    let localized_outlives_constraints = polonius_context
        .as_mut()
        .map(|polonius_context| polonius_context.create_localized_constraints(&mut regioncx, body));

    // If requested: dump NLL facts, and run legacy polonius analysis.
    let polonius_output = all_facts.as_ref().and_then(|all_facts| {
        if infcx.tcx.sess.opts.unstable_opts.nll_facts {
            let def_id = body.source.def_id();
            let def_path = infcx.tcx.def_path(def_id);
            let dir_path = PathBuf::from(&infcx.tcx.sess.opts.unstable_opts.nll_facts_dir)
                .join(def_path.to_filename_friendly_no_crate());
            all_facts.write_to_dir(dir_path, location_table).unwrap();
        }

        if polonius_output {
            let algorithm =
                env::var("POLONIUS_ALGORITHM").unwrap_or_else(|_| String::from("Hybrid"));
            let algorithm = Algorithm::from_str(&algorithm).unwrap();
            debug!("compute_regions: using polonius algorithm {:?}", algorithm);
            let _prof_timer = infcx.tcx.prof.generic_activity("polonius_analysis");
            Some(Box::new(Output::compute(all_facts, algorithm, false)))
        } else {
            None
        }
    });

    // Solve the region constraints.
    let (closure_region_requirements, nll_errors) =
        regioncx.solve(infcx, body, polonius_output.clone());

    if let Some(guar) = nll_errors.has_errors() {
        // Suppress unhelpful extra errors in `infer_opaque_types`.
        infcx.set_tainted_by_errors(guar);
    }

    let remapped_opaque_tys = regioncx.infer_opaque_types(infcx, opaque_type_values);

    NllOutput {
        regioncx,
        opaque_type_values: remapped_opaque_tys,
        polonius_input: all_facts.map(Box::new),
        polonius_output,
        opt_closure_req: closure_region_requirements,
        nll_errors,
        localized_outlives_constraints,
    }
}

/// `-Zdump-mir=nll` dumps MIR annotated with NLL specific information:
/// - free regions
/// - inferred region values
/// - region liveness
/// - inference constraints and their causes
///
/// As well as graphviz `.dot` visualizations of:
/// - the region constraints graph
/// - the region SCC graph
pub(super) fn dump_nll_mir<'tcx>(
    infcx: &BorrowckInferCtxt<'tcx>,
    body: &Body<'tcx>,
    regioncx: &RegionInferenceContext<'tcx>,
    closure_region_requirements: &Option<ClosureRegionRequirements<'tcx>>,
    borrow_set: &BorrowSet<'tcx>,
) {
    let tcx = infcx.tcx;
    if !dump_enabled(tcx, "nll", body.source.def_id()) {
        return;
    }

    // We want the NLL extra comments printed by default in NLL MIR dumps (they were removed in
    // #112346). Specifying `-Z mir-include-spans` on the CLI still has priority: for example,
    // they're always disabled in mir-opt tests to make working with blessed dumps easier.
    let options = PrettyPrintMirOptions {
        include_extra_comments: matches!(
            infcx.tcx.sess.opts.unstable_opts.mir_include_spans,
            MirIncludeSpans::On | MirIncludeSpans::Nll
        ),
    };
    dump_mir_with_options(
        tcx,
        false,
        "nll",
        &0,
        body,
        |pass_where, out| {
            emit_nll_mir(tcx, regioncx, closure_region_requirements, borrow_set, pass_where, out)
        },
        options,
    );

    // Also dump the region constraint graph as a graphviz file.
    let _: io::Result<()> = try {
        let mut file = create_dump_file(tcx, "regioncx.all.dot", false, "nll", &0, body)?;
        regioncx.dump_graphviz_raw_constraints(&mut file)?;
    };

    // Also dump the region constraint SCC graph as a graphviz file.
    let _: io::Result<()> = try {
        let mut file = create_dump_file(tcx, "regioncx.scc.dot", false, "nll", &0, body)?;
        regioncx.dump_graphviz_scc_constraints(&mut file)?;
    };
}

/// Produces the actual NLL MIR sections to emit during the dumping process.
pub(crate) fn emit_nll_mir<'tcx>(
    tcx: TyCtxt<'tcx>,
    regioncx: &RegionInferenceContext<'tcx>,
    closure_region_requirements: &Option<ClosureRegionRequirements<'tcx>>,
    borrow_set: &BorrowSet<'tcx>,
    pass_where: PassWhere,
    out: &mut dyn io::Write,
) -> io::Result<()> {
    match pass_where {
        // Before the CFG, dump out the values for each region variable.
        PassWhere::BeforeCFG => {
            regioncx.dump_mir(tcx, out)?;
            writeln!(out, "|")?;

            if let Some(closure_region_requirements) = closure_region_requirements {
                writeln!(out, "| Free Region Constraints")?;
                for_each_region_constraint(tcx, closure_region_requirements, &mut |msg| {
                    writeln!(out, "| {msg}")
                })?;
                writeln!(out, "|")?;
            }

            if borrow_set.len() > 0 {
                writeln!(out, "| Borrows")?;
                for (borrow_idx, borrow_data) in borrow_set.iter_enumerated() {
                    writeln!(
                        out,
                        "| {:?}: issued at {:?} in {:?}",
                        borrow_idx, borrow_data.reserve_location, borrow_data.region
                    )?;
                }
                writeln!(out, "|")?;
            }
        }

        PassWhere::BeforeLocation(_) => {}

        PassWhere::AfterTerminator(_) => {}

        PassWhere::BeforeBlock(_) | PassWhere::AfterLocation(_) | PassWhere::AfterCFG => {}
    }
    Ok(())
}

#[allow(rustc::diagnostic_outside_of_impl)]
#[allow(rustc::untranslatable_diagnostic)]
pub(super) fn dump_annotation<'tcx, 'infcx>(
    infcx: &'infcx BorrowckInferCtxt<'tcx>,
    body: &Body<'tcx>,
    regioncx: &RegionInferenceContext<'tcx>,
    closure_region_requirements: &Option<ClosureRegionRequirements<'tcx>>,
    opaque_type_values: &FxIndexMap<LocalDefId, OpaqueHiddenType<'tcx>>,
    diags: &mut crate::diags::BorrowckDiags<'infcx, 'tcx>,
) {
    let tcx = infcx.tcx;
    let base_def_id = tcx.typeck_root_def_id(body.source.def_id());
    if !tcx.has_attr(base_def_id, sym::rustc_regions) {
        return;
    }

    // When the enclosing function is tagged with `#[rustc_regions]`,
    // we dump out various bits of state as warnings. This is useful
    // for verifying that the compiler is behaving as expected. These
    // warnings focus on the closure region requirements -- for
    // viewing the intraprocedural state, the -Zdump-mir output is
    // better.

    let def_span = tcx.def_span(body.source.def_id());
    let mut err = if let Some(closure_region_requirements) = closure_region_requirements {
        let mut err = infcx.dcx().struct_span_note(def_span, "external requirements");

        regioncx.annotate(tcx, &mut err);

        err.note(format!(
            "number of external vids: {}",
            closure_region_requirements.num_external_vids
        ));

        // Dump the region constraints we are imposing *between* those
        // newly created variables.
        for_each_region_constraint(tcx, closure_region_requirements, &mut |msg| {
            err.note(msg);
            Ok(())
        })
        .unwrap();

        err
    } else {
        let mut err = infcx.dcx().struct_span_note(def_span, "no external requirements");
        regioncx.annotate(tcx, &mut err);

        err
    };

    if !opaque_type_values.is_empty() {
        err.note(format!("Inferred opaque type values:\n{opaque_type_values:#?}"));
    }

    diags.buffer_non_error(err);
}

fn for_each_region_constraint<'tcx>(
    tcx: TyCtxt<'tcx>,
    closure_region_requirements: &ClosureRegionRequirements<'tcx>,
    with_msg: &mut dyn FnMut(String) -> io::Result<()>,
) -> io::Result<()> {
    for req in &closure_region_requirements.outlives_requirements {
        let subject = match req.subject {
            ClosureOutlivesSubject::Region(subject) => format!("{subject:?}"),
            ClosureOutlivesSubject::Ty(ty) => {
                with_no_trimmed_paths!(format!(
                    "{}",
                    ty.instantiate(tcx, |vid| ty::Region::new_var(tcx, vid))
                ))
            }
        };
        with_msg(format!("where {}: {:?}", subject, req.outlived_free_region,))?;
    }
    Ok(())
}

pub(crate) trait ConstraintDescription {
    fn description(&self) -> &'static str;
}
