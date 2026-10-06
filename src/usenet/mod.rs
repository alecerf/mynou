//! Native bounded Usenet formats. Metadata inspection never contacts providers.
pub mod admission;
pub mod archive;
mod engine;
mod management;
pub use management::{ProbeRequest, QueueControl};
pub mod newznab;
pub mod nntp;
pub mod nzb;
pub mod queue;
mod settings;
pub mod workspace;
pub use settings::{Downloads, Server, Settings};
pub mod yenc;
