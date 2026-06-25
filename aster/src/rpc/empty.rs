//! The `Empty` payload — the request type for a no-request unary method.
//!
//! Aster's call protocol is request-carrying (one request frame per call), so a
//! method written `async fn foo(&self) -> Result<Resp>` is sugar for a call
//! whose request is `Empty`: the client sends an `Empty` frame, the server
//! ignores it. `#[aster::service]` substitutes this automatically — users never
//! name it.

use fory_derive::ForyStruct;

/// The empty request payload (zero fields). Wire identity `aster/Empty`.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/Empty")]
pub struct Empty {}
