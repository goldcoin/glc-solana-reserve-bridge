//! Bridge route identity and the fail-closed route gate.
//!
//! # Why a `Route` type exists alongside `ledger::Direction`
//!
//! [`crate::ledger::Direction`] is the *settlement* axis: every reserve
//! mutation, every state-machine transition, every signer claim and every
//! row in `bridge_requests` is keyed by it.
//!
//! [`Route`] is the *admission* axis: the set of source→destination pairs
//! this deployment is willing to talk about at all, including ones that are
//! not implemented. It is a strict superset of `Direction`.
//!
//! The two are deliberately different types, and the conversion is
//! deliberately partial:
//!
//! ```text
//! Route::GlcToSol  ->  Some(Direction::GlcToSol)
//! Route::SolToGlc  ->  Some(Direction::SolToGlc)
//! Route::GlcToRhn  ->  Some(Direction::GlcToRhn)   // Phase F
//! Route::RhnToGlc  ->  Some(Direction::RhnToGlc)   // Phase F
//! Route::SolToRhn  ->  Some(Direction::SolToRhn)   // Phase H
//! Route::RhnToSol  ->  Some(Direction::RhnToSol)   // Phase H
//! ```
//!
//! [`Route::as_direction`] returning `None` was the load-bearing security
//! property this module was built around while routes without settlement
//! machinery existed. Every function that can move value —
//! `Ledger::create_request`, every fold, every orchestrator settlement
//! phase, every attestation/vault/EIP-712 claim builder — requires a
//! `Direction`, so a route without one could not reach any of them: not
//! because a boolean was checked, but because the value needed to call
//! them could not be constructed.
//!
//! Phase F narrowed the set of direction-less routes from four to two;
//! Phase H closed it, by giving `SolToRhn` and `RhnToSol` the machinery
//! their two halves already had (the Solana deposit indexer joined to the
//! Robinhood payout engine, and the Robinhood deposit indexer joined to
//! the Solana reserve release). The function stays `Option`-returning on
//! purpose: it documents that having a `Direction` is a property a route
//! must EARN by having executable settlement, and it is what `GET
//! /chains` publishes as `implemented`. `bridge_requests.direction`'s
//! CHECK — widened to all six spellings in schema v27 — remains the
//! database's own independent copy of the same fact.
//!
//! # Having a `Direction` is not permission to move value
//!
//! This is the distinction to hold onto now that two Robinhood routes have
//! one. `as_direction` says the machinery EXISTS. Whether it may RUN is
//! decided every time, by [`RouteGate::ensure_enabled`]'s three gates
//! below — and, for anything that touches the Robinhood custody contract,
//! by a fourth gate this service does not control at all: the contract's
//! own `routeEnabled(route)`, `depositsPaused`/`payoutsPaused` and
//! `signerEpoch`, read live over `eth_call` immediately before every
//! broadcast (`crate::robinhood::calls`). A service-side flag is NECESSARY
//! and NOT SUFFICIENT; if the contract says disabled, the operation fails
//! closed regardless of what any local gate says.
//!
//! # The three-place AND
//!
//! [`RouteGate::ensure_enabled`] admits a route only when ALL THREE of the
//! following independently say yes:
//!
//! 1. **Config** — [`RoutesConfig`], from the TOML file. A missing section,
//!    a missing field, or `false` all mean disabled.
//! 2. **Ledger** — the `bridge_routes` table (see [`crate::ledger::Ledger::
//!    route_enabled`]). A missing table, a missing row, or `enabled = 0`
//!    all mean disabled.
//! 3. **Adapter capability** — [`crate::chains::ChainAdapter::capability`].
//!    An adapter that is not operational for a route means disabled,
//!    regardless of what the other two say.
//!
//! Each gate fails closed on its own, and each is evaluated on every call —
//! none is cached. An operator cannot enable a Robinhood route by editing
//! config alone, by editing the database alone, or by both together: the
//! [`crate::chains::robinhood::RobinhoodAdapter`] additionally requires a
//! fully resolved settlement configuration to be present in this process,
//! and reports [`crate::chains::Capability::Unavailable`] for any route
//! whose protocol chain pair that preflight did not read off the deployed
//! contract.
//!
//! # Legacy routes are enabled by construction, not by configuration
//!
//! `GlcToSol`/`SolToGlc` are production traffic that predates this module.
//! Their [`Route::default_enabled`] is `true`, so every gate above resolves
//! to "enabled" against an unmodified production config file and an
//! unmigrated production ledger — the existing Solana↔Goldcoin behaviour is
//! bit-for-bit unchanged. New routes default to `false`.
//!
//! That single `default_enabled` rule is what lets all three gates share
//! one fallback and lets the `bridge_routes` migration seed legacy rows to
//! `1` and Robinhood rows to `0` without changing any behaviour.

use crate::chains::{Capability, ChainRegistry};
use crate::ledger::{Direction, Ledger, LedgerError};

/// A chain this deployment knows the name of. Knowing a chain's name says
/// nothing about whether any route to it is usable — see [`RouteGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Chain {
    Goldcoin,
    Solana,
    /// Robinhood Network. Every chain parameter (family, chain id, RPC,
    /// token contract, decimals, finality) is deliberately UNRESOLVED in
    /// this phase — see `docs/30-robinhood-network-phase1.md`. Nothing in
    /// this codebase may assume any of them.
    Robinhood,
}

impl Chain {
    pub fn as_str(self) -> &'static str {
        match self {
            Chain::Goldcoin => "goldcoin",
            Chain::Solana => "solana",
            Chain::Robinhood => "robinhood",
        }
    }

    /// Operator/UI-facing name. Not an identifier — never parse this.
    pub fn display_name(self) -> &'static str {
        match self {
            Chain::Goldcoin => "Goldcoin L1",
            Chain::Solana => "Solana",
            Chain::Robinhood => "Robinhood Network",
        }
    }

    pub const ALL: [Chain; 3] = [Chain::Goldcoin, Chain::Solana, Chain::Robinhood];
}

impl std::str::FromStr for Chain {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "goldcoin" => Ok(Chain::Goldcoin),
            "solana" => Ok(Chain::Solana),
            "robinhood" => Ok(Chain::Robinhood),
            other => Err(format!("unknown chain {other:?}")),
        }
    }
}

/// A source→destination pair this deployment can be asked about.
///
/// The wire spelling is the identifier: it appears in the public API, in
/// operator tooling, and (for legacy routes only) in `bridge_requests.
/// direction`. `GlcToRhn`/`RhnToGlc` follow the existing `GlcToSol`/
/// `SolToGlc` convention deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Route {
    GlcToSol,
    SolToGlc,
    /// Goldcoin L1 → Robinhood Network. Settlement machinery exists
    /// (Phase F); the route ships **disabled** and opening it needs every
    /// gate, this service's and the contract's.
    GlcToRhn,
    /// Robinhood Network → Goldcoin L1. Settlement machinery exists
    /// (Phase F); ships **disabled**, same as its twin.
    RhnToGlc,
    /// Solana → Robinhood Network. Settlement machinery exists (Phase H:
    /// the Solana deposit indexer feeding the Robinhood payout engine);
    /// ships **disabled** on every gate, exactly like the Goldcoin pair.
    SolToRhn,
    /// Robinhood Network → Solana. Settlement machinery exists (Phase H:
    /// the Robinhood deposit indexer feeding the Solana reserve release);
    /// ships **disabled**, same as its twin.
    RhnToSol,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::GlcToSol => "GlcToSol",
            Route::SolToGlc => "SolToGlc",
            Route::GlcToRhn => "GlcToRhn",
            Route::RhnToGlc => "RhnToGlc",
            Route::SolToRhn => "SolToRhn",
            Route::RhnToSol => "RhnToSol",
        }
    }

    pub fn source_chain(self) -> Chain {
        match self {
            Route::GlcToSol | Route::GlcToRhn => Chain::Goldcoin,
            Route::SolToGlc | Route::SolToRhn => Chain::Solana,
            Route::RhnToGlc | Route::RhnToSol => Chain::Robinhood,
        }
    }

    pub fn destination_chain(self) -> Chain {
        match self {
            Route::GlcToSol | Route::RhnToSol => Chain::Solana,
            Route::SolToGlc | Route::RhnToGlc => Chain::Goldcoin,
            Route::GlcToRhn | Route::SolToRhn => Chain::Robinhood,
        }
    }

    /// The settlement [`Direction`] this route executes as, or `None` if
    /// this route has no settlement machinery.
    ///
    /// **This is the type-level firewall described in the module docs.**
    /// `None` is not "not yet wired up" — it means no `Direction` value
    /// exists for this route, so none of the reserve/ledger/signing
    /// functions that require one can be called with it at all. Do not add
    /// a total conversion, a `From` impl, an `unwrap_or`, or a default
    /// here: each would convert a compile-time guarantee into a runtime
    /// check.
    ///
    /// Returning `Some` says the machinery exists, and nothing more. It is
    /// not an enablement check and must never be used as one — see the
    /// module docs' "Having a `Direction` is not permission to move
    /// value".
    pub fn as_direction(self) -> Option<Direction> {
        match self {
            Route::GlcToSol => Some(Direction::GlcToSol),
            Route::SolToGlc => Some(Direction::SolToGlc),
            // Phase F built the settlement machinery for these two, so
            // they now have a `Direction`. Having one is NOT permission to
            // use it: `RouteGate::ensure_enabled`'s three gates, and the
            // contract's own `routeEnabled`, all still stand in front of
            // every value-moving call.
            Route::GlcToRhn => Some(Direction::GlcToRhn),
            Route::RhnToGlc => Some(Direction::RhnToGlc),
            // Phase H joined the existing Solana and Robinhood legs into
            // the two cross routes. Same caveat as above, and the same
            // gates in front of every value-moving call.
            Route::SolToRhn => Some(Direction::SolToRhn),
            Route::RhnToSol => Some(Direction::RhnToSol),
        }
    }

    /// What every gate resolves to when it holds no explicit opinion: an
    /// absent config section, an absent `bridge_routes` table, or an absent
    /// row. `true` only for the two routes that predate the route registry,
    /// so an unmodified production deployment is unaffected; `false` for
    /// everything else, so an unconfigured route is a disabled route.
    pub fn default_enabled(self) -> bool {
        match self {
            Route::GlcToSol | Route::SolToGlc => true,
            Route::GlcToRhn | Route::RhnToGlc | Route::SolToRhn | Route::RhnToSol => false,
        }
    }

    /// Whether this route existed before the route registry. Used only to
    /// document/justify [`Route::default_enabled`]; never itself a gate.
    pub fn is_legacy(self) -> bool {
        matches!(self, Route::GlcToSol | Route::SolToGlc)
    }

    /// Whether this is one of the two Solana<->Robinhood routes — the
    /// pair that gained settlement machinery last (Phase H), after every
    /// production config file had already been written. The one place
    /// this distinction is load-bearing is `[fees]` completeness
    /// ([`crate::fees::RouteFees::covers_required_routes`]): a config
    /// that predates these routes must keep loading unchanged, so a
    /// cross route may go unpriced ONLY while it is disabled in config.
    pub fn is_solana_robinhood(self) -> bool {
        matches!(self, Route::SolToRhn | Route::RhnToSol)
    }

    /// Whether an operator may write this route's `enabled` flag into the
    /// ledger's `bridge_routes` state
    /// ([`crate::ledger::Ledger::set_route_enabled`]).
    ///
    /// Exactly the four Robinhood routes, and this is a narrowing — never
    /// a gate. Saying `true` here authorizes nothing: it says only that an
    /// operator's `enabled = 1` is a MEANINGFUL row to write for this
    /// route, which the legacy pair is not.
    ///
    /// - `GlcToSol`/`SolToGlc` are excluded because their control already
    ///   exists as the pause/admission machinery
    ///   (`glc-admin pause`/`close-admission`). A second, divergent
    ///   spelling of "turn off production traffic" — one that no reserve
    ///   invariant, liquidity check or audit path knows about — is
    ///   exactly what [`RoutesConfig::with_robinhood`] refuses to add on
    ///   the config side, and this refuses it on the ledger side.
    /// - `SolToRhn`/`RhnToSol` were excluded while
    ///   [`Route::as_direction`] yielded `None` for them; since Phase H
    ///   both have settlement machinery, so an `enabled = 1` row is a
    ///   claim the rest of the system can honour. The migration seeds
    ///   them at `0`, so they stay closed until an operator opens them.
    ///
    /// The match is exhaustive on purpose: a new route variant is a
    /// compile error here until someone decides whether an operator may
    /// switch it.
    pub fn is_operator_settable(self) -> bool {
        match self {
            Route::GlcToRhn | Route::RhnToGlc | Route::SolToRhn | Route::RhnToSol => true,
            Route::GlcToSol | Route::SolToGlc => false,
        }
    }

    /// Whether an operator may write this route's ADMISSION flag into the
    /// ledger's `route_admission` state
    /// ([`crate::ledger::Ledger::set_route_admission`]).
    ///
    /// Since schema v38: EVERY route with settlement machinery — i.e.
    /// every route whose [`Route::as_direction`] is `Some`, which since
    /// Phase H is all six. Pinned against that predicate by
    /// `tests::admission_settable_is_exactly_the_routes_with_a_direction`.
    ///
    /// # This is a different axis from [`Route::is_operator_settable`]
    ///
    /// Read the two together, because reaching for the wrong one is the
    /// mistake this doc exists to prevent:
    ///
    /// ```text
    ///              is_operator_settable   is_admission_settable
    /// GlcToSol            false                  TRUE (v38)
    /// SolToGlc            false                  TRUE
    /// GlcToRhn            TRUE                   TRUE (v38)
    /// RhnToGlc            TRUE                   TRUE
    /// SolToRhn            TRUE                   TRUE
    /// RhnToSol            TRUE                   TRUE
    /// ```
    ///
    /// `is_operator_settable` governs ENABLEMENT — one of
    /// [`RouteGate`]'s three gates, i.e. "is this route switched on in
    /// this deployment". This governs ADMISSION — whether a route that
    /// IS switched on will accept a NEW transfer right now, evaluated by
    /// [`crate::ledger::InboundAdmissionGates`] alongside the reserve's
    /// own `paused`/`admission_closed`. A route must pass BOTH, and
    /// neither can substitute for the other.
    ///
    /// # Where each route's admission moment is
    ///
    /// - The four OBSERVED-deposit routes (`SolToGlc`, `RhnToGlc`,
    ///   `SolToRhn`, `RhnToSol`): the fold. A closed gate parks the newly
    ///   observed deposit in `ManualReview` with
    ///   `route_admission_closed_at_fold`, recoverable and refundable
    ///   like every other fold-time park.
    /// - The two REQUESTED-deposit routes (`GlcToSol`, `GlcToRhn`, v38):
    ///   [`crate::ledger::Ledger::create_request_from`], where
    ///   `POST /transfers` reserves the destination capacity. A closed
    ///   gate refuses the new request before any row, reservation or
    ///   deposit address exists — nothing is parked because nothing was
    ///   accepted. A request created BEFORE the gate closed keeps
    ///   settling: already-accepted obligations are never affected by
    ///   any admission flag, on any route.
    ///
    /// # Why this does not contradict `is_operator_settable`'s refusal
    ///
    /// That function excludes `GlcToSol`/`SolToGlc` because a per-route
    /// ENABLE flag would be "a second, divergent spelling of turn off
    /// production traffic — one that no reserve invariant, liquidity
    /// check or audit path knows about". This flag is the opposite of
    /// divergent: it is read by the SAME
    /// [`crate::ledger::InboundAdmissionGates`] evaluator both folds and
    /// `GET /chains` already gate on, it is written only through the same
    /// audited mutation path, and re-opening it runs the same three
    /// reserve safety checks `open-admission` runs against the route's
    /// destination reserve. It adds a narrower scope to existing
    /// machinery rather than a second mechanism beside it. Before v38 the
    /// only way to stop ONE Goldcoin-sourced route was to pause its
    /// destination reserve, which also stops the other route drawing on
    /// that reserve — the coarse control this gate exists to refine, and
    /// which it leaves exactly as it was.
    ///
    /// The match is exhaustive on purpose: a new route variant is a
    /// compile error here until someone decides whether it carries a
    /// route-level admission gate.
    pub fn is_admission_settable(self) -> bool {
        match self {
            Route::GlcToSol
            | Route::SolToGlc
            | Route::GlcToRhn
            | Route::RhnToGlc
            | Route::SolToRhn
            | Route::RhnToSol => true,
        }
    }

    /// The routes [`Route::is_admission_settable`] admits, in registry
    /// order — for the migration seed, operator listings and exhaustive
    /// iteration. Pinned against the predicate by
    /// `tests::admission_settable_list_matches_the_predicate`. Since v38
    /// this is every route, in [`Route::ALL`] order.
    pub const ADMISSION_SETTABLE: [Route; 6] = Route::ALL;

    pub const ALL: [Route; 6] = [
        Route::GlcToSol,
        Route::SolToGlc,
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ];

    /// This route's discriminator in the `GlcRobinhoodBridge` custody
    /// contract, or `None` for a route the contract does not model.
    ///
    /// The contract is Robinhood-side custody: Robinhood is one leg of
    /// every route it knows, so `GlcToSol` and `SolToGlc` have no
    /// discriminator and must return `None`. That `None` is the same kind
    /// of firewall as [`Route::as_direction`]'s — it is not "not wired up
    /// yet", it is "this value does not exist", so a Solana-only route can
    /// never be handed to contract-facing code by accident.
    ///
    /// The four byte values mirror `GlcRobinhoodBridge`'s `ROUTE_*`
    /// constants exactly and are a WIRE CONTRACT with deployed bytecode:
    /// they are never renumbered, never reordered, and `0x00` is
    /// permanently invalid on both sides. Changing one here without
    /// changing the deployed contract would silently authorize the wrong
    /// route.
    ///
    /// Returning a discriminator says nothing about whether the route is
    /// enabled — on this side or on-chain.
    pub fn contract_route_id(self) -> Option<u8> {
        match self {
            Route::GlcToSol | Route::SolToGlc => None,
            Route::GlcToRhn => Some(0x01),
            Route::RhnToGlc => Some(0x02),
            Route::SolToRhn => Some(0x03),
            Route::RhnToSol => Some(0x04),
        }
    }
}

impl std::str::FromStr for Route {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GlcToSol" => Ok(Route::GlcToSol),
            "SolToGlc" => Ok(Route::SolToGlc),
            "GlcToRhn" => Ok(Route::GlcToRhn),
            "RhnToGlc" => Ok(Route::RhnToGlc),
            "SolToRhn" => Ok(Route::SolToRhn),
            "RhnToSol" => Ok(Route::RhnToSol),
            other => Err(format!("unknown route {other:?}")),
        }
    }
}

impl From<Direction> for Route {
    /// Widening a settlement direction to its route is always total and
    /// lossless — it is only the reverse ([`Route::as_direction`]) that is
    /// partial.
    fn from(direction: Direction) -> Route {
        match direction {
            Direction::GlcToSol => Route::GlcToSol,
            Direction::SolToGlc => Route::SolToGlc,
            Direction::GlcToRhn => Route::GlcToRhn,
            Direction::RhnToGlc => Route::RhnToGlc,
            Direction::SolToRhn => Route::SolToRhn,
            Direction::RhnToSol => Route::RhnToSol,
        }
    }
}

/// Which of the three independent gates refused, and the operator-facing
/// reason. The variant is deliberately reported (rather than collapsed into
/// one opaque "disabled") so an operator debugging a route that will not
/// open can tell config from database from adapter without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisabledBy {
    Config,
    Ledger,
    Adapter { reason: String },
}

impl DisabledBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisabledBy::Config => "config",
            DisabledBy::Ledger => "ledger",
            DisabledBy::Adapter { .. } => "adapter",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouteGateError {
    /// The route is known but not open. Carries no chain detail, no
    /// balance, and no timing — a caller learns only that this route
    /// cannot be used and why in the coarsest terms.
    #[error("route {route} is not enabled ({}): {reason}", disabled_by.as_str())]
    Disabled {
        route: &'static str,
        disabled_by: DisabledBy,
        reason: String,
    },
    #[error("ledger error while resolving route state: {0}")]
    Ledger(#[from] LedgerError),
}

impl RouteGateError {
    /// Approved end-user copy for a route that exists but is not open yet.
    /// Deliberately cause-agnostic and free of any promise about when it
    /// opens — the same discipline as
    /// [`crate::api::DIRECTION_UNAVAILABLE_MESSAGE`].
    pub const UNAVAILABLE_MESSAGE: &'static str =
        "This route is not available yet.\nRobinhood Network support is in development and \
         cannot be used for transfers.";
}

/// Per-route enable flags as declared by the config file.
///
/// Built by [`crate::config::Config`]; a route with no explicit entry
/// resolves to [`Route::default_enabled`]. Deliberately not a `HashMap`
/// with a permissive `get`: the lookup is exhaustive over [`Route`], so
/// adding a route variant is a compile error here until its config
/// semantics are decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutesConfig {
    glc_to_sol: bool,
    sol_to_glc: bool,
    glc_to_rhn: bool,
    rhn_to_glc: bool,
    sol_to_rhn: bool,
    rhn_to_sol: bool,
}

impl Default for RoutesConfig {
    /// Every route at its [`Route::default_enabled`] value — i.e. exactly
    /// what an existing production config file (which names no routes at
    /// all) resolves to.
    fn default() -> Self {
        RoutesConfig {
            glc_to_sol: Route::GlcToSol.default_enabled(),
            sol_to_glc: Route::SolToGlc.default_enabled(),
            glc_to_rhn: Route::GlcToRhn.default_enabled(),
            rhn_to_glc: Route::RhnToGlc.default_enabled(),
            sol_to_rhn: Route::SolToRhn.default_enabled(),
            rhn_to_sol: Route::RhnToSol.default_enabled(),
        }
    }
}

impl RoutesConfig {
    pub fn enabled(&self, route: Route) -> bool {
        match route {
            Route::GlcToSol => self.glc_to_sol,
            Route::SolToGlc => self.sol_to_glc,
            Route::GlcToRhn => self.glc_to_rhn,
            Route::RhnToGlc => self.rhn_to_glc,
            Route::SolToRhn => self.sol_to_rhn,
            Route::RhnToSol => self.rhn_to_sol,
        }
    }

    /// Applies the optional `[robinhood]` config section's four flags. The
    /// legacy routes have no config surface at all and are not settable
    /// here — there is deliberately no way to express "turn off GlcToSol"
    /// in this struct, because that control already exists as the
    /// pause/admission machinery and must not gain a second, divergent
    /// spelling.
    ///
    /// Takes all four Robinhood routes as separate parameters rather than a
    /// struct or a "Robinhood on/off" switch: the routes are governed
    /// independently on-chain, one at a time under signer quorum, and a
    /// single switch here would be a second, coarser control that could
    /// disagree with the contract.
    pub fn with_robinhood(
        mut self,
        glc_to_rhn: bool,
        rhn_to_glc: bool,
        sol_to_rhn: bool,
        rhn_to_sol: bool,
    ) -> Self {
        self.glc_to_rhn = glc_to_rhn;
        self.rhn_to_glc = rhn_to_glc;
        self.sol_to_rhn = sol_to_rhn;
        self.rhn_to_sol = rhn_to_sol;
        self
    }
}

/// The single admission gate. Constructed once at startup and consulted on
/// every route-bearing request; holds no cached verdict.
pub struct RouteGate {
    config: RoutesConfig,
    registry: ChainRegistry,
}

impl RouteGate {
    pub fn new(config: RoutesConfig, registry: ChainRegistry) -> Self {
        RouteGate { config, registry }
    }

    /// A gate that admits exactly the two legacy routes — the resolved
    /// state of an unmodified production deployment.
    pub fn legacy_only() -> Self {
        RouteGate::new(RoutesConfig::default(), ChainRegistry::phase1())
    }

    pub fn config(&self) -> &RoutesConfig {
        &self.config
    }

    pub fn registry(&self) -> &ChainRegistry {
        &self.registry
    }

    /// The one function every entry point calls. Returns `Ok(())` only when
    /// config, ledger, and adapter capability all independently admit the
    /// route.
    ///
    /// Evaluation order is config → ledger → adapter, and it short-circuits;
    /// the order affects only which `disabled_by` an operator sees when more
    /// than one gate is closed, never whether the route opens.
    pub fn ensure_enabled(&self, ledger: &Ledger, route: Route) -> Result<(), RouteGateError> {
        // Gate 1 — config.
        if !self.config.enabled(route) {
            return Err(RouteGateError::Disabled {
                route: route.as_str(),
                disabled_by: DisabledBy::Config,
                reason: "not enabled in the service configuration".to_string(),
            });
        }

        // Gate 2 — persisted route state. A missing table or row resolves
        // to `Route::default_enabled`, so this is live today against an
        // unmigrated ledger and stays correct after the Phase-2 migration
        // seeds the table.
        if !ledger.route_enabled(route.as_str(), route.default_enabled())? {
            return Err(RouteGateError::Disabled {
                route: route.as_str(),
                disabled_by: DisabledBy::Ledger,
                reason: "disabled in the ledger's bridge_routes state".to_string(),
            });
        }

        // Gate 3 — adapter capability. Both chains must be operational for
        // this route: a route is only as usable as its weaker leg.
        for chain in [route.source_chain(), route.destination_chain()] {
            match self.registry.capability(chain, route) {
                Capability::Operational => {}
                Capability::Unavailable { reason } => {
                    return Err(RouteGateError::Disabled {
                        route: route.as_str(),
                        disabled_by: DisabledBy::Adapter {
                            reason: reason.clone(),
                        },
                        reason,
                    });
                }
            }
        }

        Ok(())
    }

    /// Non-failing form for read-only listings (`GET /chains`, `GET
    /// /status`). Never used to authorize anything — [`RouteGate::
    /// ensure_enabled`] is the only admission decision.
    ///
    /// # This is not "the route is usable right now"
    ///
    /// It is the three-place AND above and nothing else: config,
    /// `bridge_routes`, adapter capability. It does not read the reserve,
    /// so it stays `true` while the destination reserve is paused, while
    /// an operator has closed admission, and while capacity is exhausted
    /// — in every one of which a newly observed inbound deposit folds
    /// into `ManualReview` instead of settling.
    ///
    /// That gap was a production launch-blocker: `GET /chains` published
    /// this verdict as `enabled`, a UI read it as availability, and users
    /// made irreversible `RhnToGlc` deposits while
    /// `reserve_ledger.admission_closed` was set on `GoldcoinReserve`.
    /// The runtime half now lives in
    /// [`crate::ledger::InboundAdmissionGates`] and is published beside
    /// this one as `RouteView::available`. Anything choosing whether to
    /// OFFER a transfer must read that; this field answers only whether
    /// the route is switched on.
    pub fn is_enabled(&self, ledger: &Ledger, route: Route) -> bool {
        self.ensure_enabled(ledger, route).is_ok()
    }

    /// The reason a route is closed, for display. `None` when it is open.
    pub fn disabled_reason(&self, ledger: &Ledger, route: Route) -> Option<String> {
        match self.ensure_enabled(ledger, route) {
            Ok(()) => None,
            Err(RouteGateError::Disabled { .. }) => {
                Some(RouteGateError::UNAVAILABLE_MESSAGE.to_string())
            }
            // A ledger read failure is not a "reason this route is closed",
            // but it must never render as "open" either.
            Err(RouteGateError::Ledger(_)) => Some(RouteGateError::UNAVAILABLE_MESSAGE.to_string()),
        }
    }
}

#[cfg(test)]
mod tests;
