use std::collections::{BTreeMap, BTreeSet};

use troupe_diagnostics_core::{event::DiagnosticScope, scalar::SchemaU64};

use crate::{
    collect::ProjectionError,
    identity::{DenseIdentityMap, IdentitySpace, component},
};

pub(crate) const ROOT_TRACK_IDENTITY: &str = "production";

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackDefinition {
    canonical_identity: String,
    parent_identity: Option<String>,
    name: String,
    depth: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TrackCatalogBuilder {
    definitions: BTreeMap<String, TrackDefinition>,
}

impl TrackCatalogBuilder {
    pub(crate) fn new(root_name: String) -> Self {
        let mut builder = Self::default();
        builder
            .register(ROOT_TRACK_IDENTITY.to_owned(), None, root_name)
            .expect("the initial root track is unique");
        builder
    }

    pub(crate) fn register_metadata(&mut self, name: String) -> Result<String, ProjectionError> {
        let identity = format!("{ROOT_TRACK_IDENTITY}/metadata");
        self.register(
            identity.clone(),
            Some(ROOT_TRACK_IDENTITY.to_owned()),
            name,
        )?;
        Ok(identity)
    }

    pub(crate) fn register_scope(
        &mut self,
        scope: &DiagnosticScope,
    ) -> Result<String, ProjectionError> {
        let mut parent = ROOT_TRACK_IDENTITY.to_owned();
        for (tag, value, label) in scope_segments(scope) {
            let identity = format!("{parent}/{tag}:{}", component(&value));
            let name = format!("{label} {value}");
            self.register(identity.clone(), Some(parent), name)?;
            parent = identity;
        }
        Ok(parent)
    }

    pub(crate) fn register_counter(
        &mut self,
        parent: &str,
        series_identity: &str,
        name: &str,
    ) -> Result<String, ProjectionError> {
        let identity = counter_track_identity(parent, series_identity);
        self.register(identity.clone(), Some(parent.to_owned()), name.to_owned())?;
        Ok(identity)
    }

    fn register(
        &mut self,
        canonical_identity: String,
        parent_identity: Option<String>,
        name: String,
    ) -> Result<(), ProjectionError> {
        let depth = match parent_identity.as_deref() {
            Some(parent) => self
                .definitions
                .get(parent)
                .map(|definition| definition.depth + 1)
                .ok_or_else(|| ProjectionError::unknown_identity(parent))?,
            None => 0,
        };
        let definition = TrackDefinition {
            canonical_identity: canonical_identity.clone(),
            parent_identity,
            name,
            depth,
        };
        if let Some(existing) = self.definitions.get(&canonical_identity) {
            if existing != &definition {
                return Err(ProjectionError::conflicting_track(&canonical_identity));
            }
            return Ok(());
        }
        self.definitions.insert(canonical_identity, definition);
        Ok(())
    }

    pub(crate) fn finish(self, maximum_ids: u64) -> Result<TrackCatalog, ProjectionError> {
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
            }
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackDescriptorInfo<'catalog> {
    pub(crate) uuid: u64,
    pub(crate) parent_uuid: Option<u64>,
    pub(crate) name: &'catalog str,
}

#[derive(Clone, Debug)]
pub(crate) struct SpanInterval {
    pub(crate) start_sequence: SchemaU64,
    pub(crate) finish_sequence: Option<SchemaU64>,
    pub(crate) parent_track_identity: String,
    pub(crate) base_identity: String,
    pub(crate) display_name: String,
}

pub(crate) fn allocate_span_lanes(
    builder: &mut TrackCatalogBuilder,
    intervals: impl IntoIterator<Item = SpanInterval>,
) -> Result<BTreeMap<SchemaU64, String>, ProjectionError> {
    let mut groups = BTreeMap::<String, Vec<SpanInterval>>::new();
    for interval in intervals {
        groups
            .entry(interval.base_identity.clone())
            .or_default()
            .push(interval);
    }

    let mut assignments = BTreeMap::new();
    for intervals in groups.values_mut() {
        intervals.sort_by_key(|interval| interval.start_sequence);
        let mut lanes = Vec::<Vec<Option<SchemaU64>>>::new();
        for interval in intervals {
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
                .unwrap_or_else(|| {
                    lanes.push(Vec::new());
                    lanes.len() - 1
                });
            lanes[lane].push(interval.finish_sequence);

            let track_identity = format!("{}/lane:{lane:020}", interval.base_identity);
            let display_name = if lane == 0 {
                interval.display_name.clone()
            } else {
                format!("{} [lane {}]", interval.display_name, lane + 1)
            };
            builder.register(
                track_identity.clone(),
                Some(interval.parent_track_identity.clone()),
                display_name,
            )?;
            assignments.insert(interval.start_sequence, track_identity);
        }
    }
    Ok(assignments)
}
