//! Adapters translating our storage + FSM types into the traits
//! that openraft requires. Split across submodules so each piece
//! can be tested and evolved independently.

pub mod fsm;
pub mod log;

pub use fsm::AbixioFsmAdapter;
pub use log::AbixioLogAdapter;
