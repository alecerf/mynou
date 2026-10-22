//! Explicit discovery protection. Protected sources never construct a host transport.
use crate::{
    Result,
    json::Value,
    torrent::transport::{Network, Publication},
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protection {
    #[default]
    Direct,
    Protected,
}
impl Protection {
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        match v.get("protection") {
            None => Ok(Self::Direct),
            Some(Value::String(s)) if s == "direct" => Ok(Self::Direct),
            Some(Value::String(s)) if s == "protected" => Ok(Self::Protected),
            _ => Err("Source: protection requires direct or protected".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Protected => "protected",
        }
    }
    pub(crate) fn check(self, network: Option<&Arc<dyn Network>>) -> Result<()> {
        if self == Self::Protected
            && network
                .ok_or("Source: protected network unavailable")?
                .generation()?
                .is_none()
        {
            return Err("Source: protected network unavailable".into());
        }
        Ok(())
    }
    pub(crate) fn publication(
        self,
        network: Option<&Arc<dyn Network>>,
    ) -> Result<Box<dyn Publication + '_>> {
        if self == Self::Direct {
            return Ok(Box::new(()));
        }
        let network = network.ok_or("Source: protected network unavailable")?;
        let lease = network.publication()?;
        self.check(Some(network))?;
        Ok(lease)
    }
}
impl crate::engine::Engine {
    pub(crate) fn source_publication(
        &self,
        protection: Protection,
    ) -> Result<Box<dyn Publication + '_>> {
        let lease = protection.publication(Some(&self.torrent_network))?;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Source: operation stopped".into());
        }
        Ok(lease)
    }
}

pub(crate) mod direct;

#[cfg(test)]
mod tests;
