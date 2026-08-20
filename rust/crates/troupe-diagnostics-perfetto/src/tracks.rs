use std::collections::{BTreeMap, BTreeSet};

use troupe_diagnostics_core::{event::DiagnosticScope, scalar::SchemaU64};

use crate::{
    collect::{ProjectionError, StructuralIndexBudget},
    identity::{DenseIdentityMap, IdentitySpace, component},
};

pub(crate) const ROOT_TRACK_IDENTITY: &str = "production";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackKind {
    Timeline,
    Counter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackDefinition {
    canonical_identity: String,
    parent_identity: Option<String>,
    name: String,
    depth: usize,
    kind: TrackKind,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TrackCatalogBuilder {
    definitions: BTreeMap<String, TrackDefinition>,
}

impl TrackCatalogBuilder {
    pub(crate) fn new(
        root_name: String,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<Self, ProjectionError> {
        let mut builder = Self::default();
        builder.register(
            ROOT_TRACK_IDENTITY,
            None,
            &root_name,
            TrackKind::Timeline,
            structural_budget,
        )?;
        Ok(builder)
    }

    pub(crate) fn register_metadata(
        &mut self,
        name: String,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<String, ProjectionError> {
        let identity = format!("{ROOT_TRACK_IDENTITY}/metadata");
        self.register(
            &identity,
            Some(ROOT_TRACK_IDENTITY),
            &name,
            TrackKind::Timeline,
            structural_budget,
        )?;
        Ok(identity)
    }

    pub(crate) fn register_scope(
        &mut self,
        scope: &DiagnosticScope,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<String, ProjectionError> {
        let mut parent = ROOT_TRACK_IDENTITY.to_owned();
        for (tag, value, label) in scope_segments(scope) {
            let identity = format!("{parent}/{tag}:{}", component(&value));
            let name = format!("{label} {value}");
            self.register(
                &identity,
                Some(&parent),
                &name,
                TrackKind::Timeline,
                structural_budget,
            )?;
            parent = identity;
        }
        Ok(parent)
    }

    pub(crate) fn register_counter(
        &mut self,
        parent: &str,
        series_identity: &str,
        name: &str,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<String, ProjectionError> {
        let identity = counter_track_identity(parent, series_identity);
        self.register(
            &identity,
            Some(parent),
            name,
            TrackKind::Counter,
            structural_budget,
        )?;
        Ok(identity)
    }

    fn register(
        &mut self,
        canonical_identity: &str,
        parent_identity: Option<&str>,
        name: &str,
        kind: TrackKind,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<(), ProjectionError> {
        let depth = match parent_identity {
            Some(parent) => self
                .definitions
                .get(parent)
                .map(|definition| definition.depth + 1)
                .ok_or_else(|| ProjectionError::unknown_identity(parent))?,
            None => 0,
        };
        if let Some(existing) = self.definitions.get(canonical_identity) {
            if existing.parent_identity.as_deref() != parent_identity
                || existing.name != name
                || existing.depth != depth
                || existing.kind != kind
            {
                return Err(ProjectionError::conflicting_track(canonical_identity));
            }
            return Ok(());
        }
        structural_budget.reserve_owned(
            1,
            [
                Some(canonical_identity),
                Some(canonical_identity),
                parent_identity,
                Some(name),
            ]
            .into_iter()
            .flatten(),
        )?;
        let definition = TrackDefinition {
            canonical_identity: canonical_identity.to_owned(),
            parent_identity: parent_identity.map(str::to_owned),
            name: name.to_owned(),
            depth,
            kind,
        };
        self.definitions
            .insert(canonical_identity.to_owned(), definition);
        Ok(())
    }

    pub(crate) fn finish(
        self,
        maximum_ids: u64,
        structural_budget: &mut StructuralIndexBudget,
    ) -> Result<TrackCatalog, ProjectionError> {
        for identity in self.definitions.keys() {
            structural_budget.reserve_owned(1, [identity.as_str()])?;
            structural_budget.reserve_owned(1, [identity.as_str()])?;
        }
        let identities = self.definitions.keys().cloned().collect::<BTreeSet<_>>();
        let ids = DenseIdentityMap::assign(identities, IdentitySpace::Track, maximum_ids)?;
        let mut descriptor_order = self.definitions.keys().cloned().collect::<Vec<_>>();
        descriptor_order.sort_by(|left, right| {
            let left = &self.definitions[left];
            let right = &self.definitions[right];
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.canonical_identity.cmp(&right.canonical_identity))
        });
        Ok(TrackCatalog {
            definitions: self.definitions,
            ids,
            descriptor_order,
        })
    }
}

pub(crate) fn scope_track_identity(scope: &DiagnosticScope) -> String {
    scope_segments(scope)
        .into_iter()
        .fold(ROOT_TRACK_IDENTITY.to_owned(), |parent, (tag, value, _)| {
            format!("{parent}/{tag}:{}", component(&value))
        })
}

pub(crate) fn counter_track_identity(parent: &str, series_identity: &str) -> String {
    format!("{parent}/counter:{}", component(series_identity))
}

fn scope_segments(scope: &DiagnosticScope) -> Vec<(&'static str, String, &'static str)> {
    let mut segments = Vec::new();
    if let Some(value) = scope.scene_id() {
        segments.push(("scene", value.as_str().to_owned(), "Scene"));
    }
    if let Some(value) = scope.actor_id() {
        segments.push(("actor", value.as_str().to_owned(), "Actor"));
    }
    if let Some(value) = scope.cue_id() {
        segments.push(("cue", value.as_str().to_owned(), "Cue"));
    }
    if let Some(value) = scope.effect_id() {
        segments.push(("effect", value.as_str().to_owned(), "Effect"));
    }
    if let Some(value) = scope.act_id() {
        segments.push(("act", value.as_str().to_owned(), "Act"));
    }
    if let Some(value) = scope.tool_call_id() {
        segments.push(("tool", value.as_str().to_owned(), "Tool"));
    }
    if let Some(value) = scope.session_generation() {
        segments.push(("session", value.get().to_string(), "Session generation"));
    }
    segments
}

#[derive(Clone, Debug)]
pub(crate) struct TrackCatalog {
    definitions: BTreeMap<String, TrackDefinition>,
    ids: DenseIdentityMap,
    descriptor_order: Vec<String>,
}

impl TrackCatalog {
    pub(crate) fn id(&self, identity: &str) -> Result<u64, ProjectionError> {
        self.ids.id(identity)
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn descriptors(&self) -> impl Iterator<Item = TrackDescriptorInfo<'_>> {
        self.descriptor_order.iter().map(|identity| {
            let definition = &self.definitions[identity];
            TrackDescriptorInfo {
                uuid: self
                    .ids
                    .id(&definition.canonical_identity)
                    .expect("finished catalog assigns every descriptor"),
                parent_uuid: definition.parent_identity.as_deref().map(|parent| {
                    self.ids
                        .id(parent)
                        .expect("finished catalog assigns every descriptor parent")
                }),
                name: &definition.name,
                is_counter: definition.kind == TrackKind::Counter,
            }
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackDescriptorInfo<'catalog> {
    pub(crate) uuid: u64,
    pub(crate) parent_uuid: Option<u64>,
    pub(crate) name: &'catalog str,
    pub(crate) is_counter: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SpanInterval {
    pub(crate) start_sequence: SchemaU64,
    pub(crate) finish_sequence: Option<SchemaU64>,
    pub(crate) parent_track_identity: String,
    pub(crate) role: String,
    pub(crate) display_name: String,
}

pub(crate) fn allocate_span_lanes(
    builder: &mut TrackCatalogBuilder,
    intervals: impl IntoIterator<Item = SpanInterval>,
    structural_budget: &mut StructuralIndexBudget,
) -> Result<BTreeMap<SchemaU64, String>, ProjectionError> {
    let mut intervals = intervals.into_iter().collect::<Vec<_>>();
    intervals.sort_by(|left, right| {
        left.parent_track_identity
            .cmp(&right.parent_track_identity)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.start_sequence.cmp(&right.start_sequence))
    });
    let mut assignments = BTreeMap::new();
    let mut group_start = 0;
    while group_start < intervals.len() {
        let mut group_end = group_start + 1;
        while group_end < intervals.len()
            && intervals[group_end].parent_track_identity
                == intervals[group_start].parent_track_identity
            && intervals[group_end].role == intervals[group_start].role
        {
            group_end += 1;
        }
        let mut lanes = Vec::<Vec<Option<SchemaU64>>>::new();
        for interval in &intervals[group_start..group_end] {
            let lane = lanes
                .iter_mut()
                .enumerate()
                .find_map(|(index, stack)| {
                    while stack
                        .last()
                        .and_then(|finish| *finish)
                        .is_some_and(|finish| finish < interval.start_sequence)
                    {
                        stack.pop();
                    }
                    let fits = match (stack.last().copied(), interval.finish_sequence) {
                        (None, _) | (Some(None), _) => true,
                        (Some(Some(parent_finish)), Some(finish)) => finish <= parent_finish,
                        (Some(Some(_)), None) => false,
                    };
                    fits.then_some(index)
                })
                .unwrap_or(lanes.len());

            let base_identity = format!(
                "{}/span:{}",
                interval.parent_track_identity,
                component(&interval.role)
            );
            let track_identity = format!("{base_identity}/lane:{lane:020}");
            let sibling_name =
                (lane != 0).then(|| format!("{} [lane {}]", interval.display_name, lane + 1));
            let display_name = sibling_name.as_deref().unwrap_or(&interval.display_name);
            builder.register(
                &track_identity,
                Some(&interval.parent_track_identity),
                display_name,
                TrackKind::Timeline,
                structural_budget,
            )?;
            structural_budget.reserve_owned(1, [track_identity.as_str()])?;
            if lane == lanes.len() {
                lanes.push(Vec::new());
            }
            lanes[lane].push(interval.finish_sequence);
            assignments.insert(interval.start_sequence, track_identity.to_owned());
        }
        group_start = group_end;
    }
    Ok(assignments)
}
