pub mod protocol;
pub mod v1;

#[cfg(target_arch = "wasm32")]
pub mod wasm_client;

#[cfg(not(target_arch = "wasm32"))]
pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod client_v1;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
