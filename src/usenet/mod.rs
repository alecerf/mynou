//! Native bounded Usenet formats. Metadata inspection never contacts providers.
mod management;
pub use management::ProbeRequest;
pub mod nntp;
pub mod nzb;
mod settings;
pub mod workspace;
pub use settings::{Server, Settings};
pub mod yenc;
