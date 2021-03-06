use crate::dep_graph::{self, DepConstructor, DepNode, DepNodeParams};
use crate::hir::exports::Export;
use crate::hir::map;
use crate::infer::canonical::{self, Canonical};
use crate::lint::LintLevelMap;
use crate::middle::codegen_fn_attrs::CodegenFnAttrs;
use crate::middle::cstore::{CrateSource, DepKind, NativeLibraryKind};
use crate::middle::cstore::{ExternCrate, ForeignModule, LinkagePreference, NativeLibrary};
use crate::middle::exported_symbols::{ExportedSymbol, SymbolExportLevel};
use crate::middle::lang_items::{LangItem, LanguageItems};
use crate::middle::lib_features::LibFeatures;
use crate::middle::privacy::AccessLevels;
use crate::middle::region;
use crate::middle::resolve_lifetime::{ObjectLifetimeDefault, Region, ResolveLifetimes};
use crate::middle::stability::{self, DeprecationEntry};
use crate::mir;
use crate::mir::interpret::GlobalId;
use crate::mir::interpret::{ConstEvalRawResult, ConstEvalResult, ConstValue};
use crate::mir::interpret::{LitToConstError, LitToConstInput};
use crate::mir::mono::CodegenUnit;
use crate::traits::query::{
    CanonicalPredicateGoal, CanonicalProjectionGoal, CanonicalTyGoal,
    CanonicalTypeOpAscribeUserTypeGoal, CanonicalTypeOpEqGoal, CanonicalTypeOpNormalizeGoal,
    CanonicalTypeOpProvePredicateGoal, CanonicalTypeOpSubtypeGoal, NoSolution,
};
use crate::traits::query::{
    DropckOutlivesResult, DtorckConstraint, MethodAutoderefStepsResult, NormalizationResult,
    OutlivesBound,
};
use crate::traits::specialization_graph;
use crate::traits::Clauses;
use crate::traits::{self, Vtable};
use crate::ty::steal::Steal;
use crate::ty::subst::SubstsRef;
use crate::ty::util::AlwaysRequiresDrop;
use crate::ty::{self, AdtSizedConstraint, CrateInherentImpls, ParamEnvAnd, Ty, TyCtxt};
use crate::util::common::ErrorReported;
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexMap};
use rustc_data_structures::profiling::ProfileCategory::*;
use rustc_data_structures::stable_hasher::StableVec;
use rustc_data_structures::svh::Svh;
use rustc_data_structures::sync::Lrc;
use rustc_hir as hir;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CrateNum, DefId, DefIdMap, DefIdSet, LocalDefId};
use rustc_hir::{Crate, HirIdSet, ItemLocalId, TraitCandidate};
use rustc_index::vec::IndexVec;
use rustc_session::config::{EntryFnType, OptLevel, OutputFilenames, SymbolManglingVersion};
use rustc_session::CrateDisambiguator;
use rustc_target::spec::PanicStrategy;

use rustc_ast::ast;
use rustc_attr as attr;
use rustc_span::symbol::Symbol;
use rustc_span::{Span, DUMMY_SP};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

#[macro_use]
mod plumbing;
pub(crate) use self::plumbing::CycleError;
use self::plumbing::*;

mod stats;
pub use self::stats::print_stats;

mod job;
#[cfg(parallel_compiler)]
pub use self::job::handle_deadlock;
use self::job::QueryJobInfo;
pub use self::job::{QueryInfo, QueryJob, QueryJobId};

mod keys;
use self::keys::Key;

mod values;
use self::values::Value;

mod caches;
use self::caches::CacheSelector;

mod config;
use self::config::QueryAccessors;
pub use self::config::QueryConfig;
pub(crate) use self::config::QueryDescription;

mod on_disk_cache;
pub use self::on_disk_cache::OnDiskCache;

mod profiling_support;
pub use self::profiling_support::{IntoSelfProfilingString, QueryKeyStringBuilder};

// Each of these queries corresponds to a function pointer field in the
// `Providers` struct for requesting a value of that type, and a method
// on `tcx: TyCtxt` (and `tcx.at(span)`) for doing that request in a way
// which memoizes and does dep-graph tracking, wrapping around the actual
// `Providers` that the driver creates (using several `rustc_*` crates).
//
// The result type of each query must implement `Clone`, and additionally
// `ty::query::values::Value`, which produces an appropriate placeholder
// (error) value if the query resulted in a query cycle.
// Queries marked with `fatal_cycle` do not need the latter implementation,
// as they will raise an fatal error on query cycles instead.

rustc_query_append! { [define_queries!][<'tcx>] }

/// The red/green evaluation system will try to mark a specific DepNode in the
/// dependency graph as green by recursively trying to mark the dependencies of
/// that `DepNode` as green. While doing so, it will sometimes encounter a `DepNode`
/// where we don't know if it is red or green and we therefore actually have
/// to recompute its value in order to find out. Since the only piece of
/// information that we have at that point is the `DepNode` we are trying to
/// re-evaluate, we need some way to re-run a query from just that. This is what
/// `force_from_dep_node()` implements.
///
/// In the general case, a `DepNode` consists of a `DepKind` and an opaque
/// GUID/fingerprint that will uniquely identify the node. This GUID/fingerprint
/// is usually constructed by computing a stable hash of the query-key that the
/// `DepNode` corresponds to. Consequently, it is not in general possible to go
/// back from hash to query-key (since hash functions are not reversible). For
/// this reason `force_from_dep_node()` is expected to fail from time to time
/// because we just cannot find out, from the `DepNode` alone, what the
/// corresponding query-key is and therefore cannot re-run the query.
///
/// The system deals with this case letting `try_mark_green` fail which forces
/// the root query to be re-evaluated.
///
/// Now, if `force_from_dep_node()` would always fail, it would be pretty useless.
/// Fortunately, we can use some contextual information that will allow us to
/// reconstruct query-keys for certain kinds of `DepNode`s. In particular, we
/// enforce by construction that the GUID/fingerprint of certain `DepNode`s is a
/// valid `DefPathHash`. Since we also always build a huge table that maps every
/// `DefPathHash` in the current codebase to the corresponding `DefId`, we have
/// everything we need to re-run the query.
///
/// Take the `mir_validated` query as an example. Like many other queries, it
/// just has a single parameter: the `DefId` of the item it will compute the
/// validated MIR for. Now, when we call `force_from_dep_node()` on a `DepNode`
/// with kind `MirValidated`, we know that the GUID/fingerprint of the `DepNode`
/// is actually a `DefPathHash`, and can therefore just look up the corresponding
/// `DefId` in `tcx.def_path_hash_to_def_id`.
///
/// When you implement a new query, it will likely have a corresponding new
/// `DepKind`, and you'll have to support it here in `force_from_dep_node()`. As
/// a rule of thumb, if your query takes a `DefId` or `LocalDefId` as sole parameter,
/// then `force_from_dep_node()` should not fail for it. Otherwise, you can just
/// add it to the "We don't have enough information to reconstruct..." group in
/// the match below.
pub fn force_from_dep_node<'tcx>(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> bool {
    use crate::dep_graph::DepKind;

    // We must avoid ever having to call `force_from_dep_node()` for a
    // `DepNode::codegen_unit`:
    // Since we cannot reconstruct the query key of a `DepNode::codegen_unit`, we
    // would always end up having to evaluate the first caller of the
    // `codegen_unit` query that *is* reconstructible. This might very well be
    // the `compile_codegen_unit` query, thus re-codegenning the whole CGU just
    // to re-trigger calling the `codegen_unit` query with the right key. At
    // that point we would already have re-done all the work we are trying to
    // avoid doing in the first place.
    // The solution is simple: Just explicitly call the `codegen_unit` query for
    // each CGU, right after partitioning. This way `try_mark_green` will always
    // hit the cache instead of having to go through `force_from_dep_node`.
    // This assertion makes sure, we actually keep applying the solution above.
    debug_assert!(
        dep_node.kind != DepKind::codegen_unit,
        "calling force_from_dep_node() on DepKind::codegen_unit"
    );

    if !dep_node.kind.can_reconstruct_query_key() {
        return false;
    }

    rustc_dep_node_force!([dep_node, tcx]
        // These are inputs that are expected to be pre-allocated and that
        // should therefore always be red or green already.
        DepKind::CrateMetadata |

        // These are anonymous nodes.
        DepKind::TraitSelect |

        // We don't have enough information to reconstruct the query key of
        // these.
        DepKind::CompileCodegenUnit => {
            bug!("force_from_dep_node: encountered {:?}", dep_node)
        }
    );

    false
}

impl DepNode {
    /// Check whether the query invocation corresponding to the given
    /// DepNode is eligible for on-disk-caching. If so, this is method
    /// will execute the query corresponding to the given DepNode.
    /// Also, as a sanity check, it expects that the corresponding query
    /// invocation has been marked as green already.
    pub fn try_load_from_on_disk_cache<'tcx>(&self, tcx: TyCtxt<'tcx>) {
        use crate::dep_graph::DepKind;

        rustc_dep_node_try_load_from_on_disk_cache!(self, tcx)
    }
}
