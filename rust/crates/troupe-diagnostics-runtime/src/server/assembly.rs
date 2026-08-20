use hyper::Method;
use troupe_diagnostics_core::id::CanonicalUuid;

use super::{
    assets,
    dump::DumpEndpoints,
    error::RouteConfigurationError,
    query::{EVENTS_PATH, QueryEndpoints, SNAPSHOT_PATH, STATUS_PATH},
    routes::{RouteDefinition, validate_route_definitions},
    sse::replay::{SseEndpoint, requests_event_stream},
};

const RUN_ID_MISMATCH: &str = "assembled diagnostic endpoints belong to different Runs";

#[derive(Clone)]
pub struct ActiveRouteAssembly {
    run_id: CanonicalUuid,
    queries: QueryEndpoints,
    sse: SseEndpoint,
    dump: DumpEndpoints,
}

impl ActiveRouteAssembly {
    pub fn new(
        queries: QueryEndpoints,
        sse: SseEndpoint,
        dump: DumpEndpoints,
    ) -> Result<Self, RouteConfigurationError> {
        let run_id = queries.run_id();
        validate_run_ids(run_id, [sse.run_id(), dump.run_id()])?;
        Ok(Self {
            run_id,
            queries,
            sse,
            dump,
        })
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn route_definitions(&self) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
        let queries = self.queries.clone();
        let sse = self.sse.clone();
        let events = RouteDefinition::read_only(EVENTS_PATH, move |request| {
            let queries = queries.clone();
            let sse = sse.clone();
            async move {
                let response = if request.method() == Method::GET && requests_event_stream(&request)
                {
                    sse.handle_follow(request)
                } else {
                    queries.handle_finite_events(request)
                };
                Ok(response)
            }
        })?;
        assemble_routes(&self.queries, events, &self.dump)
    }
}

#[derive(Clone)]
pub struct ArchiveRouteAssembly {
    run_id: CanonicalUuid,
    queries: QueryEndpoints,
    dump: DumpEndpoints,
}

impl ArchiveRouteAssembly {
    pub fn new(
        queries: QueryEndpoints,
        dump: DumpEndpoints,
    ) -> Result<Self, RouteConfigurationError> {
        let run_id = queries.run_id();
        validate_run_ids(run_id, [dump.run_id()])?;
        Ok(Self {
            run_id,
            queries,
            dump,
        })
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn route_definitions(&self) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
        let queries = self.queries.clone();
        let events = RouteDefinition::read_only(EVENTS_PATH, move |request| {
            let queries = queries.clone();
            async move { Ok(queries.handle_finite_events(request)) }
        })?;
        assemble_routes(&self.queries, events, &self.dump)
    }
}

fn validate_run_ids<const N: usize>(
    expected: CanonicalUuid,
    actual: [CanonicalUuid; N],
) -> Result<(), RouteConfigurationError> {
    if actual.into_iter().all(|candidate| candidate == expected) {
        Ok(())
    } else {
        Err(RouteConfigurationError::new(RUN_ID_MISMATCH))
    }
}

fn assemble_routes(
    queries: &QueryEndpoints,
    events: RouteDefinition,
    dump: &DumpEndpoints,
) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
    let status = queries.clone();
    let snapshot = queries.clone();
    let mut routes = vec![
        RouteDefinition::read_only(STATUS_PATH, move |request| {
            let endpoint = status.clone();
            async move { Ok(endpoint.handle_status(request)) }
        })?,
        RouteDefinition::read_only(SNAPSHOT_PATH, move |request| {
            let endpoint = snapshot.clone();
            async move { Ok(endpoint.handle_snapshot(request)) }
        })?,
        events,
    ];
    routes.extend(dump.route_definitions()?);
    routes.extend(assets::route_definitions()?);
    validate_route_definitions(&routes)?;
    Ok(routes)
}
