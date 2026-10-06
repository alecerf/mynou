//! Native bounded Usenet formats. Metadata inspection never contacts providers.
mod management;
pub use management::{ProbeRequest, QueueControl};
pub mod nntp;
pub mod newznab;
pub mod nzb;
pub mod queue;
mod settings;
pub mod workspace;
pub use settings::{Downloads, Server, Settings};
pub mod yenc;
