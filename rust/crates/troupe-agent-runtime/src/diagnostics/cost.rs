use std::{fmt, sync::Arc};

use agent_client_protocol::schema::v1::{Cost as AcpCost, SessionUpdate};
use troupe_diagnostics_core::scalar::{CurrencyCode, DecimalString};

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{
        AgentDiagnosticProvider, AgentDiagnosticUpdateContext, AgentSessionDiagnosticContext,
        AgentSessionDiagnosticMetadata,
    },
};

pub const AGENT_COST_CANDIDATE_KIND: &str = "agent_cumulative_cost";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentCostNormalizationError {
    AmountWithoutCurrency,
    CurrencyWithoutAmount,
    NonFiniteAmount,
    NegativeAmount,
    InvalidAmount,
    InvalidCurrency,
}

impl AgentCostNormalizationError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::AmountWithoutCurrency => "cost_amount_without_currency",
            Self::CurrencyWithoutAmount => "cost_currency_without_amount",
            Self::NonFiniteAmount => "cost_amount_nonfinite",
            Self::NegativeAmount => "cost_amount_negative",
            Self::InvalidAmount => "cost_amount_invalid",
            Self::InvalidCurrency => "cost_currency_invalid",
        }
    }
}

impl fmt::Display for AgentCostNormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AgentCostNormalizationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCumulativeCost {
    amount: DecimalString,
    currency: CurrencyCode,
}

impl AgentCumulativeCost {
    pub fn from_decimal_pair(
        amount: Option<&str>,
        currency: Option<&str>,
    ) -> Result<Option<Self>, AgentCostNormalizationError> {
        let (amount, currency) = match (amount, currency) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err(AgentCostNormalizationError::AmountWithoutCurrency);
            }
            (None, Some(_)) => {
                return Err(AgentCostNormalizationError::CurrencyWithoutAmount);
            }
            (Some(amount), Some(currency)) => (amount, currency),
        };

        if is_nonfinite_decimal(amount) {
            return Err(AgentCostNormalizationError::NonFiniteAmount);
        }
        let amount =
            DecimalString::parse(amount).map_err(|_| AgentCostNormalizationError::InvalidAmount)?;
        if amount.as_str().starts_with('-') {
            return Err(AgentCostNormalizationError::NegativeAmount);
        }
        let currency = CurrencyCode::parse(currency)
            .map_err(|_| AgentCostNormalizationError::InvalidCurrency)?;
        Ok(Some(Self { amount, currency }))
    }

    pub fn from_acp(cost: Option<&AcpCost>) -> Result<Option<Self>, AgentCostNormalizationError> {
        let Some(cost) = cost else {
            return Ok(None);
        };
        if !cost.amount.is_finite() {
            return Err(AgentCostNormalizationError::NonFiniteAmount);
        }
        if cost.amount < 0.0 {
            return Err(AgentCostNormalizationError::NegativeAmount);
        }

        let amount = cost.amount.to_string();
        Self::from_decimal_pair(Some(&amount), Some(&cost.currency))
    }

    pub const fn amount(&self) -> &DecimalString {
        &self.amount
    }

    pub const fn currency(&self) -> &CurrencyCode {
        &self.currency
    }
}

fn is_nonfinite_decimal(amount: &str) -> bool {
    matches!(
        amount,
        "NaN" | "+NaN" | "-NaN" | "Infinity" | "+Infinity" | "-Infinity"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCostDetail {
    session: AgentSessionDiagnosticContext,
    session_generation: Option<u64>,
    provider: AgentDiagnosticProvider,
    effective_model: Option<Arc<str>>,
}

impl AgentCostDetail {
    pub fn new(
        session: AgentSessionDiagnosticContext,
        session_generation: Option<u64>,
        provider: AgentDiagnosticProvider,
        effective_model: Option<Arc<str>>,
    ) -> Self {
        Self {
            session,
            session_generation,
            provider,
            effective_model,
        }
    }

    fn from_session_metadata(metadata: &AgentSessionDiagnosticMetadata) -> Self {
        Self::new(
            metadata.context().clone(),
            metadata.generation(),
            metadata.provider(),
            metadata.effective_model().map(Arc::from),
        )
    }

    pub const fn session(&self) -> &AgentSessionDiagnosticContext {
        &self.session
    }

    pub const fn session_generation(&self) -> Option<u64> {
        self.session_generation
    }

    pub const fn provider(&self) -> AgentDiagnosticProvider {
        self.provider
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCostCandidate {
    detail: AgentCostDetail,
    cost: AgentCumulativeCost,
}

impl AgentCostCandidate {
    pub const fn new(detail: AgentCostDetail, cost: AgentCumulativeCost) -> Self {
        Self { detail, cost }
    }

    pub const fn detail(&self) -> &AgentCostDetail {
        &self.detail
    }

    pub const fn cost(&self) -> &AgentCumulativeCost {
        &self.cost
    }
}

impl AgentDiagnosticCandidate for AgentCostCandidate {
    fn kind(&self) -> &'static str {
        AGENT_COST_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let SessionUpdate::UsageUpdate(usage) = update else {
        return;
    };
    let Ok(Some(cost)) = AgentCumulativeCost::from_acp(usage.cost.as_ref()) else {
        return;
    };
    let Some(metadata) = context.session.as_deref() else {
        return;
    };
    let candidate = AgentCostCandidate::new(AgentCostDetail::from_session_metadata(metadata), cost);
    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(candidate)));
}
