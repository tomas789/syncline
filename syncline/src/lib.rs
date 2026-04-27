pub mod ignore;
pub mod protocol;
pub mod v1;
/// WASM-client retry-sweep predicate. Lives at the crate root so the
/// pure logic compiles + tests on every target, even though the only
/// caller is wasm-gated `wasm_client_v1`.
pub(crate) mod blob_retry;

#[cfg(target_arch = "wasm32")]
pub mod wasm_client;
#[cfg(target_arch = "wasm32")]
pub mod wasm_client_v1;

#[cfg(not(target_arch = "wasm32"))]
pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod client_v1;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
