mod id;
mod runtime;
mod runtime_registry;
pub mod state;
mod title;
mod wait_lease;

pub use id::TerminalId;
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
pub(crate) use wait_lease::{
    acquire_wait_lease, complete_wait_lease_request, pending_wait_lease_requests,
    release_wait_lease, WaitLeaseOperation, WaitLeaseRequest, WaitLeaseResponse,
    WaitLeaseResponseResult, WAIT_LEASE_POLL_INTERVAL,
};
pub use wait_lease::{ActiveWaitLease, WaitLease, MAX_WAIT_LEASE_TTL_MS};
